// Implementation reference: https://github.com/neovim/neovim/blob/f2906a4669a2eef6d7bf86a29648793d63c98949/runtime/autoload/provider/clipboard.vim#L68-L152

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use thiserror::Error;

#[derive(Clone, Copy)]
pub enum ClipboardType {
    Clipboard,
    Selection,
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[cfg(windows)]
    #[error("Windows API error: {0}")]
    WinAPI(#[from] clipboard_win::ErrorCode),
    #[error("clipboard provider command failed")]
    CommandFailed,
    #[error("failed to write to clipboard provider's stdin")]
    StdinWriteFailed,
}

type Result<T> = std::result::Result<T, ClipboardError>;

#[cfg(not(target_arch = "wasm32"))]
pub use external::ClipboardProvider;
#[cfg(target_arch = "wasm32")]
pub use noop::ClipboardProvider;

// Clipboard not supported for wasm
#[cfg(target_arch = "wasm32")]
mod noop {
    use super::*;

    #[derive(Debug, Clone)]
    pub enum ClipboardProvider {}

    impl ClipboardProvider {
        pub fn detect() -> Self {
            Self
        }

        pub fn name(&self) -> Cow<str> {
            "none".into()
        }

        pub fn set_contents(&self, _content: &str, _clipboard_type: ClipboardType) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod external {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Command {
        command: Cow<'static, str>,
        #[serde(default)]
        args: Cow<'static, [Cow<'static, str>]>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "kebab-case")]
    pub struct CommandProvider {
        paste: Command,
        paste_primary: Option<Command>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "kebab-case")]
    #[allow(clippy::large_enum_variant)]
    pub enum ClipboardProvider {
        Pasteboard,
        Wayland,
        XClip,
        XSel,
        Win32Yank,
        Tmux,
        #[cfg(windows)]
        Windows,
        Termux,

        Termcode,
        Custom(CommandProvider),
        None,
    }

    impl Default for ClipboardProvider {
        #[cfg(windows)]
        fn default() -> Self {
            use helix_stdx::env::binary_exists;

            if binary_exists("win32yank.exe") {
                Self::Win32Yank
            } else {
                Self::Windows
            }
        }

        #[cfg(target_os = "macos")]
        fn default() -> Self {
            use helix_stdx::env::{binary_exists, env_var_is_set};

            if env_var_is_set("TMUX") && binary_exists("tmux") {
                Self::Tmux
            } else if binary_exists("pbcopy") {
                Self::Pasteboard
            } else {
                return Self::Termcode;
            }
        }

        #[cfg(not(any(windows, target_os = "macos")))]
        fn default() -> Self {
            use helix_stdx::env::{binary_exists, env_var_is_set};

            if env_var_is_set("WAYLAND_DISPLAY") && binary_exists("wl-copy") {
                Self::Wayland
            } else if env_var_is_set("DISPLAY") && binary_exists("xclip") {
                Self::XClip
            } else if env_var_is_set("DISPLAY") && binary_exists("xsel") {
                Self::XSel
            } else if binary_exists("termux-clipboard-set") {
                Self::Termux
            } else if env_var_is_set("TMUX") && binary_exists("tmux") {
                Self::Tmux
            } else if binary_exists("win32yank.exe") {
                Self::Win32Yank
            } else {
                Self::Termcode
            }
        }
    }

    impl ClipboardProvider {
        pub fn name(&self) -> Cow<'_, str> {
            fn builtin_name<'a>(
                name: &'static str,
                provider: &'static CommandProvider,
            ) -> Cow<'a, str> {
                Cow::Owned(format!("{} ({})", name, provider.paste.command))
            }

            match self {
                // These names should match the config option names from Serde
                Self::Pasteboard => builtin_name("pasteboard", &PASTEBOARD),
                Self::Wayland => builtin_name("wayland", &WL_CLIPBOARD),
                Self::XClip => builtin_name("x-clip", &XCLIP),
                Self::XSel => builtin_name("x-sel", &XSEL),
                Self::Win32Yank => builtin_name("win32-yank", &WIN32),
                Self::Tmux => builtin_name("tmux", &TMUX),
                Self::Termux => builtin_name("termux", &TERMUX),
                #[cfg(windows)]
                Self::Windows => "windows".into(),
        
                Self::Termcode => "termcode".into(),
                Self::Custom(command_provider) => Cow::Owned(format!(
                    "custom ({})",
                    command_provider.paste.command
                )),
                Self::None => "none".into(),
            }
        }

        pub fn set_contents(&self, content: &str, clipboard_type: ClipboardType) -> Result<()> {
            fn paste_to_builtin(
                provider: CommandProvider,
                content: &str,
                clipboard_type: ClipboardType,
            ) -> Result<()> {
                let cmd = match clipboard_type {
                    ClipboardType::Clipboard => &provider.paste,
                    ClipboardType::Selection => {
                        if let Some(cmd) = provider.paste_primary.as_ref() {
                            cmd
                        } else {
                            return Ok(());
                        }
                    }
                };

                execute_command(cmd, Some(content))
            }

            match self {
                Self::Pasteboard => paste_to_builtin(PASTEBOARD, content, clipboard_type),
                Self::Wayland => paste_to_builtin(WL_CLIPBOARD, content, clipboard_type),
                Self::XClip => paste_to_builtin(XCLIP, content, clipboard_type),
                Self::XSel => paste_to_builtin(XSEL, content, clipboard_type),
                Self::Win32Yank => paste_to_builtin(WIN32, content, clipboard_type),
                Self::Tmux => paste_to_builtin(TMUX, content, clipboard_type),
                Self::Termux => paste_to_builtin(TERMUX, content, clipboard_type),
                #[cfg(target_os = "windows")]
                Self::Windows => match clipboard_type {
                    ClipboardType::Clipboard => {
                        clipboard_win::set_clipboard(clipboard_win::formats::Unicode, content)?;
                        Ok(())
                    }
                    ClipboardType::Selection => Ok(()),
                },
        
                Self::Termcode => {
                    use std::io::Write;
                    let selection = match clipboard_type {
                        ClipboardType::Clipboard => "c",
                        ClipboardType::Selection => "p",
                    };
                    // OSC 52 escape sequence to set clipboard (base64 encoded)
                    let encoded = simple_base64_encode(content.as_bytes());
                    let mut stdout = std::io::stdout().lock();
                    write!(stdout, "\x1b]52;{};{}\x07", selection, encoded)?;
                    stdout.flush()?;
                    Ok(())
                }
                Self::Custom(command_provider) => match clipboard_type {
                    ClipboardType::Clipboard => {
                        execute_command(&command_provider.paste, Some(content))
                    }
                    ClipboardType::Selection => {
                        if let Some(cmd) = &command_provider.paste_primary {
                            execute_command(cmd, Some(content))
                        } else {
                            Ok(())
                        }
                    }
                },
                Self::None => Ok(()),
            }
        }
    }

    macro_rules! command_provider {
        ($name:ident,
         paste => $paste_cmd:literal $( , $paste_arg:literal )* ; ) => {
            const $name: CommandProvider = CommandProvider {
                paste: Command {
                    command: Cow::Borrowed($paste_cmd),
                    args: Cow::Borrowed(&[ $( Cow::Borrowed($paste_arg) ),* ])
                },
                paste_primary: None,
            };
        };
        ($name:ident,
         paste => $paste_cmd:literal $( , $paste_arg:literal )* ;
         paste_primary => $paste_primary_cmd:literal $( , $paste_primary_arg:literal )* ; ) => {
            const $name: CommandProvider = CommandProvider {
                paste: Command {
                    command: Cow::Borrowed($paste_cmd),
                    args: Cow::Borrowed(&[ $( Cow::Borrowed($paste_arg) ),* ])
                },
                paste_primary: Some(Command {
                    command: Cow::Borrowed($paste_primary_cmd),
                    args: Cow::Borrowed(&[ $( Cow::Borrowed($paste_primary_arg) ),* ])
                }),
            };
        };
    }

    command_provider! {
        TMUX,
        paste => "tmux", "load-buffer", "-w", "-";
    }
    command_provider! {
        PASTEBOARD,
        paste => "pbcopy";
    }
    command_provider! {
        WL_CLIPBOARD,
        paste => "wl-copy", "--type", "text/plain";
        paste_primary => "wl-copy", "-p", "--type", "text/plain";
    }
    command_provider! {
        XCLIP,
        paste => "xclip", "-i", "-selection", "clipboard";
        paste_primary => "xclip", "-i";
    }
    command_provider! {
        XSEL,
        paste => "xsel", "-i", "-b";
        paste_primary => "xsel", "-i";
    }
    command_provider! {
        WIN32,
        paste => "win32yank.exe", "-i", "--crlf";
    }
    command_provider! {
        TERMUX,
        paste => "termux-clipboard-set";
    }

    fn execute_command(cmd: &Command, input: Option<&str>) -> Result<()> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let stdin = input.map(|_| Stdio::piped()).unwrap_or_else(Stdio::null);

        let mut command: Command = Command::new(cmd.command.as_ref());

        #[allow(unused_mut)]
        let mut command_mut: &mut Command = command
            .args(cmd.args.iter().map(AsRef::as_ref))
            .stdin(stdin)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Fix for https://github.com/helix-editor/helix/issues/5424
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            unsafe {
                command_mut = command_mut.pre_exec(|| match libc::setsid() {
                    -1 => Err(std::io::Error::last_os_error()),
                    _ => Ok(()),
                });
            }
        }

        let mut child = command_mut.spawn()?;

        if let Some(input) = input {
            let mut stdin = child.stdin.take().ok_or(ClipboardError::StdinWriteFailed)?;
            stdin
                .write_all(input.as_bytes())
                .map_err(|_| ClipboardError::StdinWriteFailed)?;
        }

        // TODO: add timer?
        let output = child.wait_with_output()?;

        if !output.status.success() {
            log::error!(
                "clipboard provider {} failed with stderr: \"{}\"",
                cmd.command,
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(ClipboardError::CommandFailed);
        }

        Ok(())
    }
}

/// Simple base64 encoder for OSC 52 clipboard sequences.
fn simple_base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}
