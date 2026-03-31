use crate::{
    annotations::diagnostics::{DiagnosticFilter, InlineDiagnosticsConfig},
    clipboard::ClipboardProvider,
    document::Mode,
    events::DocumentFocusLost,
    graphics::{CursorKind, Rect},
    handlers::Handlers,
    info::Info,
    register::Registers,
    tab::Tab,
    theme::{self, Theme},
    tree,
    Document, AppId, View, ViewId,
};
use helix_event::dispatch;
use helix_vcs::DiffProviderRegistry;

use futures_util::{future, StreamExt};
use helix_lsp::{Call, LanguageServerId};

use std::{
    borrow::Cow,
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs, io,
    num::NonZeroU8,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    time::{sleep, Duration, Instant, Sleep},
};

pub use helix_core::diagnostic::Severity;
use helix_core::{
    diagnostic::DiagnosticProvider,
    syntax::{
        self,
        config::{IndentationHeuristic, LanguageServerFeature, SoftWrap},
    },
    LineEnding, Position, Range, Selection, Uri, NATIVE_LINE_ENDING,
};
use helix_lsp::lsp;

use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

use arc_swap::{
    access::{DynAccess, DynGuard},
    ArcSwap,
};

pub const DIR_STACK_CAP: usize = 10;

fn deserialize_duration_millis<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let millis = u64::deserialize(deserializer)?;
    Ok(Duration::from_millis(millis))
}

fn serialize_duration_millis<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_u64(
        duration
            .as_millis()
            .try_into()
            .map_err(|_| serde::ser::Error::custom("duration value overflowed u64"))?,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GutterConfig {
    /// Gutter Layout
    pub layout: Vec<GutterType>,
    /// Options specific to the "line-numbers" gutter
    pub line_numbers: GutterLineNumbersConfig,
}

impl Default for GutterConfig {
    fn default() -> Self {
        Self {
            layout: vec![
                GutterType::Diagnostics,
                GutterType::Spacer,
                GutterType::LineNumbers,
                GutterType::Spacer,
                GutterType::Diff,
            ],
            line_numbers: GutterLineNumbersConfig::default(),
        }
    }
}

impl From<Vec<GutterType>> for GutterConfig {
    fn from(x: Vec<GutterType>) -> Self {
        GutterConfig {
            layout: x,
            ..Default::default()
        }
    }
}

fn deserialize_gutter_seq_or_struct<'de, D>(deserializer: D) -> Result<GutterConfig, D::Error>
where
    D: Deserializer<'de>,
{
    struct GutterVisitor;

    impl<'de> serde::de::Visitor<'de> for GutterVisitor {
        type Value = GutterConfig;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                formatter,
                "an array of gutter names or a detailed gutter configuration"
            )
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut gutters = Vec::new();
            while let Some(gutter) = seq.next_element::<String>()? {
                gutters.push(
                    gutter
                        .parse::<GutterType>()
                        .map_err(serde::de::Error::custom)?,
                )
            }

            Ok(gutters.into())
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let deserializer = serde::de::value::MapAccessDeserializer::new(map);
            Deserialize::deserialize(deserializer)
        }
    }

    deserializer.deserialize_any(GutterVisitor)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct GutterLineNumbersConfig {
    /// Minimum number of characters to use for line number gutter. Defaults to 3.
    pub min_width: usize,
}

impl Default for GutterLineNumbersConfig {
    fn default() -> Self {
        Self { min_width: 3 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct FilePickerConfig {
    /// IgnoreOptions
    /// Enables ignoring hidden files.
    /// Whether to hide hidden files in file picker and global search results. Defaults to true.
    pub hidden: bool,
    /// Enables following symlinks.
    /// Whether to follow symbolic links in file picker and file or directory completions. Defaults to true.
    pub follow_symlinks: bool,
    /// Hides symlinks that point into the current directory. Defaults to true.
    pub deduplicate_links: bool,
    /// Enables reading ignore files from parent directories. Defaults to true.
    pub parents: bool,
    /// Enables reading `.ignore` files.
    /// Whether to hide files listed in .ignore in file picker and global search results. Defaults to true.
    pub ignore: bool,
    /// Enables reading `.gitignore` files.
    /// Whether to hide files listed in .gitignore in file picker and global search results. Defaults to true.
    pub git_ignore: bool,
    /// Enables reading global .gitignore, whose path is specified in git's config: `core.excludefile` option.
    /// Whether to hide files listed in global .gitignore in file picker and global search results. Defaults to true.
    pub git_global: bool,
    /// Enables reading `.git/info/exclude` files.
    /// Whether to hide files listed in .git/info/exclude in file picker and global search results. Defaults to true.
    pub git_exclude: bool,
    /// WalkBuilder options
    /// Maximum Depth to recurse directories in file picker and global search. Defaults to `None`.
    pub max_depth: Option<usize>,
}

impl Default for FilePickerConfig {
    fn default() -> Self {
        Self {
            hidden: true,
            follow_symlinks: true,
            deduplicate_links: true,
            parents: true,
            ignore: true,
            git_ignore: true,
            git_global: true,
            git_exclude: true,
            max_depth: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct FileExplorerConfig {
    /// IgnoreOptions
    /// Enables ignoring hidden files.
    /// Whether to hide hidden files in file explorer and global search results. Defaults to false.
    pub hidden: bool,
    /// Enables following symlinks.
    /// Whether to follow symbolic links in file picker and file or directory completions. Defaults to false.
    pub follow_symlinks: bool,
    /// Enables reading ignore files from parent directories. Defaults to false.
    pub parents: bool,
    /// Enables reading `.ignore` files.
    /// Whether to hide files listed in .ignore in file picker and global search results. Defaults to false.
    pub ignore: bool,
    /// Enables reading `.gitignore` files.
    /// Whether to hide files listed in .gitignore in file picker and global search results. Defaults to false.
    pub git_ignore: bool,
    /// Enables reading global .gitignore, whose path is specified in git's config: `core.excludefile` option.
    /// Whether to hide files listed in global .gitignore in file picker and global search results. Defaults to false.
    pub git_global: bool,
    /// Enables reading `.git/info/exclude` files.
    /// Whether to hide files listed in .git/info/exclude in file picker and global search results. Defaults to false.
    pub git_exclude: bool,
    /// Whether to flatten single-child directories in file explorer. Defaults to true.
    pub flatten_dirs: bool,
}

impl Default for FileExplorerConfig {
    fn default() -> Self {
        Self {
            hidden: false,
            follow_symlinks: false,
            parents: false,
            ignore: false,
            git_ignore: false,
            git_global: false,
            git_exclude: false,
            flatten_dirs: true,
        }
    }
}

fn serialize_alphabet<S>(alphabet: &[char], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let alphabet: String = alphabet.iter().collect();
    serializer.serialize_str(&alphabet)
}

fn deserialize_alphabet<'de, D>(deserializer: D) -> Result<Vec<char>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let str = String::deserialize(deserializer)?;
    let chars: Vec<_> = str.chars().collect();
    let unique_chars: HashSet<_> = chars.iter().copied().collect();
    if unique_chars.len() != chars.len() {
        return Err(<D::Error as Error>::custom(
            "jump-label-alphabet must contain unique characters",
        ));
    }
    Ok(chars)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    /// Padding to keep between the edge of the screen and the cursor when scrolling. Defaults to 5.
    pub scrolloff: usize,
    /// Number of lines to scroll at once. Defaults to 3
    pub scroll_lines: isize,
    /// Mouse support. Defaults to true.
    pub mouse: bool,
    /// Shell to use for shell commands. Defaults to ["cmd", "/C"] on Windows and ["sh", "-c"] otherwise.
    pub shell: Vec<String>,
    /// Line number mode.
    pub line_number: LineNumber,
    /// Highlight the lines cursors are currently on. Defaults to false.
    pub cursorline: bool,
    /// Highlight the columns cursors are currently on. Defaults to false.
    pub cursorcolumn: bool,
    #[serde(deserialize_with = "deserialize_gutter_seq_or_struct")]
    pub gutters: GutterConfig,
    /// Automatically reload buffers when the underlying file changes on disk. Defaults to true.
    pub auto_reload: bool,
    /// Set a global text_width
    pub text_width: usize,
    /// Time in milliseconds since last keypress before idle timers trigger.
    /// Used for various UI timeouts. Defaults to 250ms.
    #[serde(
        serialize_with = "serialize_duration_millis",
        deserialize_with = "deserialize_duration_millis"
    )]
    pub idle_timeout: Duration,
    /// Whether to display infoboxes. Defaults to true.
    pub auto_info: bool,
    pub file_picker: FilePickerConfig,
    pub file_explorer: FileExplorerConfig,
    /// Configuration of the statusline elements
    pub statusline: StatusLineConfig,
    /// Shape for cursor in each mode
    pub cursor_shape: CursorShapeConfig,
    /// Set to `true` to override automatic detection of terminal truecolor support in the event of a false negative. Defaults to `false`.
    pub true_color: bool,
    /// Set to `true` to override automatic detection of terminal undercurl support in the event of a false negative. Defaults to `false`.
    pub undercurl: bool,
    /// Search configuration.
    #[serde(default)]
    pub search: SearchConfig,
    pub lsp: LspConfig,
    pub terminal: Option<TerminalConfig>,
    /// Column numbers at which to draw the rulers. Defaults to `[]`, meaning no rulers.
    pub rulers: Vec<u16>,
    #[serde(default)]
    pub whitespace: WhitespaceConfig,
    /// Persistently display open buffers along the top
    pub bufferline: BufferLine,
    /// Vertical indent width guides.
    pub indent_guides: IndentGuidesConfig,
    /// Whether to color modes with different colors. Defaults to `false`.
    pub color_modes: bool,
    pub soft_wrap: SoftWrap,
    /// Workspace specific lsp ceiling dirs
    pub workspace_lsp_roots: Vec<PathBuf>,
    /// Which line ending to choose for new documents. Defaults to `native`. i.e. `crlf` on Windows, otherwise `lf`.
    pub default_line_ending: LineEndingConfig,
    /// Whether to automatically insert a trailing line-ending on write if missing. Defaults to `true`.
    pub insert_final_newline: bool,
    /// Whether to use atomic operations to write documents to disk.
    /// This prevents data loss if the editor is interrupted while writing the file, but may
    /// confuse some file watching/hot reloading programs. Defaults to `true`.
    pub atomic_save: bool,
    /// Whether to automatically remove all trailing line-endings after the final one on write.
    /// Defaults to `false`.
    pub trim_final_newlines: bool,
    /// Whether to automatically remove all whitespace characters preceding line-endings on write.
    /// Defaults to `false`.
    pub trim_trailing_whitespace: bool,
    /// Enables smart tab
    pub smart_tab: Option<SmartTabConfig>,
    /// Draw border around popups.
    pub popup_border: PopupBorderConfig,
    /// Which indent heuristic to use when a new line is inserted
    #[serde(default)]
    pub indent_heuristic: IndentationHeuristic,
    /// labels characters used in jumpmode
    #[serde(
        serialize_with = "serialize_alphabet",
        deserialize_with = "deserialize_alphabet"
    )]
    pub jump_label_alphabet: Vec<char>,
    /// Display diagnostic below the line they occur.
    pub inline_diagnostics: InlineDiagnosticsConfig,
    pub end_of_line_diagnostics: DiagnosticFilter,
    // Set to override the default clipboard provider
    pub clipboard_provider: ClipboardProvider,
    /// Whether to read settings from [EditorConfig](https://editorconfig.org) files. Defaults to
    /// `true`.
    pub editor_config: bool,
    /// Whether to render rainbow colors for matching brackets. Defaults to `false`.
    pub rainbow_brackets: bool,
    /// Whether to display nerdfont icons in buffer tabs and file pickers. Defaults to `false`.
    pub icons: bool,
    /// Whether to enable Kitty Keyboard Protocol
    pub kitty_keyboard_protocol: KittyKeyboardProtocolConfig,
    pub buffer_picker: BufferPickerConfig,
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub struct BufferPickerConfig {
    pub start_position: PickerStartPosition,
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum PickerStartPosition {
    #[default]
    Current,
    Previous,
}

impl PickerStartPosition {
    #[must_use]
    pub fn is_previous(self) -> bool {
        matches!(self, Self::Previous)
    }

    #[must_use]
    pub fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum KittyKeyboardProtocolConfig {
    #[default]
    Auto,
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Eq, PartialOrd, Ord)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct SmartTabConfig {
    pub enable: bool,
    pub supersede_menu: bool,
}

impl Default for SmartTabConfig {
    fn default() -> Self {
        SmartTabConfig {
            enable: true,
            supersede_menu: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct TerminalConfig {
    pub command: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[cfg(windows)]
pub fn get_terminal_provider() -> Option<TerminalConfig> {
    use helix_stdx::env::binary_exists;

    if binary_exists("wt") {
        return Some(TerminalConfig {
            command: "wt".to_string(),
            args: vec![
                "new-tab".to_string(),
                "--title".to_string(),
                "DEBUG".to_string(),
                "cmd".to_string(),
                "/C".to_string(),
            ],
        });
    }

    Some(TerminalConfig {
        command: "conhost".to_string(),
        args: vec!["cmd".to_string(), "/C".to_string()],
    })
}

#[cfg(not(any(windows, target_arch = "wasm32")))]
pub fn get_terminal_provider() -> Option<TerminalConfig> {
    use helix_stdx::env::{binary_exists, env_var_is_set};

    if env_var_is_set("TMUX") && binary_exists("tmux") {
        return Some(TerminalConfig {
            command: "tmux".to_string(),
            args: vec!["split-window".to_string()],
        });
    }

    if env_var_is_set("WEZTERM_UNIX_SOCKET") && binary_exists("wezterm") {
        return Some(TerminalConfig {
            command: "wezterm".to_string(),
            args: vec!["cli".to_string(), "split-pane".to_string()],
        });
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct LspConfig {
    /// Enables LSP
    pub enable: bool,
    /// Display LSP messagess from $/progress below statusline
    pub display_progress_messages: bool,
    /// Display LSP messages from window/showMessage below statusline
    pub display_messages: bool,
    /// Enable automatic pop up of signature help (parameter hints)
    pub auto_signature_help: bool,
    /// Display docs under signature help popup
    pub display_signature_help_docs: bool,
    /// Display inlay hints
    pub display_inlay_hints: bool,
    /// Automatically highlight symbol references at the cursor.
    pub auto_document_highlight: bool,
    /// Maximum displayed length of inlay hints (excluding the added trailing `…`).
    /// If it's `None`, there's no limit
    pub inlay_hints_length_limit: Option<NonZeroU8>,
    /// Display document color swatches
    pub display_color_swatches: bool,
    /// Whether to include declaration in the goto reference query
    pub goto_reference_include_declaration: bool,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enable: true,
            display_progress_messages: false,
            display_messages: true,
            auto_signature_help: true,
            display_signature_help_docs: true,
            display_inlay_hints: false,
            auto_document_highlight: false,
            inlay_hints_length_limit: None,
            goto_reference_include_declaration: true,
            display_color_swatches: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Smart case: Case insensitive searching unless pattern contains upper case characters. Defaults to true.
    pub smart_case: bool,
    /// Whether the search should wrap after depleting the matches. Default to true.
    pub wrap_around: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct StatusLineConfig {
    pub left: Vec<StatusLineElement>,
    pub center: Vec<StatusLineElement>,
    pub right: Vec<StatusLineElement>,
    pub separator: String,
    pub mode: ModeConfig,
    pub diagnostics: Vec<Severity>,
    pub workspace_diagnostics: Vec<Severity>,
}

impl Default for StatusLineConfig {
    fn default() -> Self {
        use StatusLineElement as E;

        Self {
            left: vec![
                E::Mode,
                E::Spinner,
                E::GitWorktree,
                E::FileName,
                E::FileModificationIndicator,
            ],
            center: vec![],
            right: vec![
                E::Diagnostics,
                E::Selections,
                E::Register,
                E::Position,
                E::FileEncoding,
            ],
            separator: String::from("│"),
            mode: ModeConfig::default(),
            diagnostics: vec![Severity::Warning, Severity::Error],
            workspace_diagnostics: vec![Severity::Warning, Severity::Error],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct ModeConfig {
    pub normal: String,
    pub insert: String,
    pub select: String,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            normal: String::from("NOR"),
            insert: String::from("INS"),
            select: String::from("SEL"),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusLineElement {
    /// The editor mode (Normal, Insert, Visual/Selection)
    Mode,

    /// The LSP activity spinner
    Spinner,

    /// The file basename (the leaf of the open file's path)
    FileBaseName,

    /// The relative file path
    FileName,

    /// The file absolute path
    FileAbsolutePath,

    // The file modification indicator
    FileModificationIndicator,

    /// An indicator that shows `"[readonly]"` when a file cannot be written
    ReadOnlyIndicator,

    /// The file encoding
    FileEncoding,

    /// The file line endings (CRLF or LF)
    FileLineEnding,

    /// The file indentation style
    FileIndentStyle,

    /// The file type (language ID or "text")
    FileType,

    /// A summary of the number of errors and warnings
    Diagnostics,

    /// A summary of the number of errors and warnings on file and workspace
    WorkspaceDiagnostics,

    /// The number of selections (cursors)
    Selections,

    /// The number of characters currently in primary selection
    PrimarySelectionLength,

    /// The cursor position
    Position,

    /// The separator string
    Separator,

    /// The cursor position as a percent of the total file
    PositionPercentage,

    /// The total line numbers of the current file
    TotalLineNumbers,

    /// A single space
    Spacer,

    /// Current version control information
    VersionControl,

    /// The git worktree name (repo root directory name)
    GitWorktree,

    /// Indicator for selected register
    Register,

    /// The base of current working directory
    CurrentWorkingDirectory,
}

// Cursor shape is read and used on every rendered frame and so needs
// to be fast. Therefore we avoid a hashmap and use an enum indexed array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShapeConfig([CursorKind; 2]);

impl CursorShapeConfig {
    pub fn from_mode(&self, mode: Mode) -> CursorKind {
        self.get(mode as usize).copied().unwrap_or_default()
    }
}

impl<'de> Deserialize<'de> for CursorShapeConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let m = HashMap::<Mode, CursorKind>::deserialize(deserializer)?;
        let into_cursor = |mode: Mode| m.get(&mode).copied().unwrap_or_default();
        Ok(CursorShapeConfig([
            into_cursor(Mode::Normal),
            into_cursor(Mode::Select),
        ]))
    }
}

impl Serialize for CursorShapeConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        let modes = [Mode::Normal, Mode::Select];
        for mode in modes {
            map.serialize_entry(&mode, &self.from_mode(mode))?;
        }
        map.end()
    }
}

impl std::ops::Deref for CursorShapeConfig {
    type Target = [CursorKind; 2];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for CursorShapeConfig {
    fn default() -> Self {
        Self([CursorKind::Block; 2])
    }
}

/// bufferline render modes
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BufferLine {
    /// Don't render bufferline
    #[default]
    Never,
    /// Always render
    Always,
    /// Only if multiple buffers are open
    Multiple,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineNumber {
    /// Show absolute line number
    Absolute,

    /// If focused and in normal/select mode, show relative line number to the primary cursor.
    /// If unfocused or in insert mode, show absolute line number.
    Relative,
}

impl std::str::FromStr for LineNumber {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "absolute" | "abs" => Ok(Self::Absolute),
            "relative" | "rel" => Ok(Self::Relative),
            _ => anyhow::bail!("Line number can only be `absolute` or `relative`."),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GutterType {
    /// Show diagnostics and other features like breakpoints
    Diagnostics,
    /// Show line numbers
    LineNumbers,
    /// Show one blank space
    Spacer,
    /// Highlight local changes
    Diff,
}

impl std::str::FromStr for GutterType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "diagnostics" => Ok(Self::Diagnostics),
            "spacer" => Ok(Self::Spacer),
            "line-numbers" => Ok(Self::LineNumbers),
            "diff" => Ok(Self::Diff),
            _ => anyhow::bail!(
                "Gutter type can only be `diagnostics`, `spacer`, `line-numbers` or `diff`."
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhitespaceConfig {
    pub render: WhitespaceRender,
    pub characters: WhitespaceCharacters,
}

impl Default for WhitespaceConfig {
    fn default() -> Self {
        Self {
            render: WhitespaceRender::Basic(WhitespaceRenderValue::None),
            characters: WhitespaceCharacters::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, rename_all = "kebab-case")]
pub enum WhitespaceRender {
    Basic(WhitespaceRenderValue),
    Specific {
        default: Option<WhitespaceRenderValue>,
        space: Option<WhitespaceRenderValue>,
        nbsp: Option<WhitespaceRenderValue>,
        nnbsp: Option<WhitespaceRenderValue>,
        tab: Option<WhitespaceRenderValue>,
        newline: Option<WhitespaceRenderValue>,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhitespaceRenderValue {
    None,
    // TODO
    // Selection,
    All,
}

impl WhitespaceRender {
    pub fn space(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, space, .. } => {
                space.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn nbsp(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, nbsp, .. } => {
                nbsp.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn nnbsp(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, nnbsp, .. } => {
                nnbsp.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn tab(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific { default, tab, .. } => {
                tab.or(default).unwrap_or(WhitespaceRenderValue::None)
            }
        }
    }
    pub fn newline(&self) -> WhitespaceRenderValue {
        match *self {
            Self::Basic(val) => val,
            Self::Specific {
                default, newline, ..
            } => newline.or(default).unwrap_or(WhitespaceRenderValue::None),
        }
    }
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhitespaceCharacters {
    pub space: char,
    pub nbsp: char,
    pub nnbsp: char,
    pub tab: char,
    pub tabpad: char,
    pub newline: char,
}

impl Default for WhitespaceCharacters {
    fn default() -> Self {
        Self {
            space: '·',   // U+00B7
            nbsp: '⍽',    // U+237D
            nnbsp: '␣',   // U+2423
            tab: '→',     // U+2192
            newline: '⏎', // U+23CE
            tabpad: ' ',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct IndentGuidesConfig {
    pub render: bool,
    pub character: char,
    pub skip_levels: u8,
}

impl Default for IndentGuidesConfig {
    fn default() -> Self {
        Self {
            skip_levels: 0,
            render: false,
            character: '│',
        }
    }
}

/// Line ending configuration.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineEndingConfig {
    /// The platform's native line ending.
    ///
    /// `crlf` on Windows, otherwise `lf`.
    #[default]
    Native,
    /// Line feed.
    LF,
    /// Carriage return followed by line feed.
    Crlf,
    /// Form feed.
    #[cfg(feature = "unicode-lines")]
    FF,
    /// Carriage return.
    #[cfg(feature = "unicode-lines")]
    CR,
    /// Next line.
    #[cfg(feature = "unicode-lines")]
    Nel,
}

impl From<LineEndingConfig> for LineEnding {
    fn from(line_ending: LineEndingConfig) -> Self {
        match line_ending {
            LineEndingConfig::Native => NATIVE_LINE_ENDING,
            LineEndingConfig::LF => LineEnding::LF,
            LineEndingConfig::Crlf => LineEnding::Crlf,
            #[cfg(feature = "unicode-lines")]
            LineEndingConfig::FF => LineEnding::FF,
            #[cfg(feature = "unicode-lines")]
            LineEndingConfig::CR => LineEnding::CR,
            #[cfg(feature = "unicode-lines")]
            LineEndingConfig::Nel => LineEnding::Nel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PopupBorderConfig {
    None,
    All,
    Popup,
    Menu,
}


impl Default for Config {
    fn default() -> Self {
        Self {
            scrolloff: 5,
            scroll_lines: 3,
            mouse: true,
            shell: if cfg!(windows) {
                vec!["cmd".to_owned(), "/C".to_owned()]
            } else {
                vec!["sh".to_owned(), "-c".to_owned()]
            },
            line_number: LineNumber::Absolute,
            cursorline: false,
            cursorcolumn: false,
            gutters: GutterConfig::default(),
            auto_reload: true,
            idle_timeout: Duration::from_millis(250),
            auto_info: true,
            file_picker: FilePickerConfig::default(),
            file_explorer: FileExplorerConfig::default(),
            statusline: StatusLineConfig::default(),
            cursor_shape: CursorShapeConfig::default(),
            true_color: false,
            undercurl: false,
            search: SearchConfig::default(),
            lsp: LspConfig::default(),
            terminal: get_terminal_provider(),
            rulers: Vec::new(),
            whitespace: WhitespaceConfig::default(),
            bufferline: BufferLine::default(),
            indent_guides: IndentGuidesConfig::default(),
            color_modes: false,
            soft_wrap: SoftWrap {
                enable: Some(false),
                ..SoftWrap::default()
            },
            text_width: 80,
            workspace_lsp_roots: Vec::new(),
            default_line_ending: LineEndingConfig::default(),
            insert_final_newline: true,
            atomic_save: true,
            trim_final_newlines: false,
            trim_trailing_whitespace: false,
            smart_tab: Some(SmartTabConfig::default()),
            popup_border: PopupBorderConfig::None,
            indent_heuristic: IndentationHeuristic::default(),
            jump_label_alphabet: ('a'..='z').collect(),
            inline_diagnostics: InlineDiagnosticsConfig::default(),
            end_of_line_diagnostics: DiagnosticFilter::Enable(Severity::Hint),
            clipboard_provider: ClipboardProvider::default(),
            editor_config: true,
            rainbow_brackets: false,
            icons: false,
            kitty_keyboard_protocol: Default::default(),
            buffer_picker: BufferPickerConfig::default(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            wrap_around: true,
            smart_case: true,
        }
    }
}

type Diagnostics = BTreeMap<Uri, Vec<(lsp::Diagnostic, DiagnosticProvider)>>;

pub struct Editor {
    /// Per-tab state: each tab owns a document, view tree, mode, etc.
    pub tabs: Vec<Box<dyn Tab>>,
    /// Index of the currently active tab.
    pub active_tab: usize,

    pub registers: Registers,
    pub language_servers: helix_lsp::Registry,
    pub diagnostics: Diagnostics,
    pub diff_providers: DiffProviderRegistry,

    pub syn_loader: Arc<ArcSwap<syntax::Loader>>,
    pub theme_loader: Arc<theme::Loader>,
    /// last_theme is used for theme previews. We store the current theme here,
    /// and if previewing is cancelled, we can return to it.
    pub last_theme: Option<Theme>,
    /// The currently applied editor theme. While previewing a theme, the previewed theme
    /// is set here.
    pub theme: Theme,

    pub status_msg: Option<(Cow<'static, str>, Severity)>,
    pub autoinfo: Option<Info>,

    pub config: Arc<dyn DynAccess<Config> + Send + Sync>,

    pub idle_timer: Pin<Box<Sleep>>,
    redraw_timer: Pin<Box<Sleep>>,
    last_motion: Option<Motion>,
    pub last_cwd: Option<PathBuf>,
    pub dir_stack: VecDeque<PathBuf>,

    pub exit_code: i32,
    /// Set to true when the editor should exit (e.g. :quit).
    pub should_exit: bool,

    pub config_events: (UnboundedSender<ConfigEvent>, UnboundedReceiver<ConfigEvent>),
    pub needs_redraw: bool,
    pub handlers: Handlers,

    /// Opaque layer state, managed by the UI layer (helix-term).
    /// Stores the compositor layer stack, downcasted by helix-term.
    pub layer_state: Box<dyn std::any::Any>,
}

pub type Motion = Box<dyn Fn(&mut Editor)>;

#[derive(Debug)]
pub enum EditorEvent {
    ConfigEvent(ConfigEvent),
    LanguageServerMessage((LanguageServerId, Call)),
    IdleTimer,
    Redraw,
}

#[derive(Debug, Clone)]
pub enum ConfigEvent {
    Refresh,
    Update(Box<Config>),
    ThemeChanged,
}

enum ThemeAction {
    Set,
    Preview,
}

#[derive(Debug, Copy, Clone)]
pub enum Action {
    Load,
    Replace,
    HorizontalSplit,
    VerticalSplit,
}

impl Action {
    /// Whether to align the view to the cursor after executing this action
    pub fn align_view(&self, view: &View, new_doc: AppId) -> bool {
        !matches!((self, view.doc == new_doc), (Action::Load, false))
    }
}

/// Error thrown on failed document closed
pub enum CloseError {
    /// Document doesn't exist
    DoesNotExist,
    /// Buffer is modified
    BufferModified(String),
    /// Document failed to save
    SaveError(anyhow::Error),
}

impl Editor {
    pub fn new(
        mut area: Rect,
        theme_loader: Arc<theme::Loader>,
        syn_loader: Arc<ArcSwap<syntax::Loader>>,
        config: Arc<dyn DynAccess<Config> + Send + Sync>,
        handlers: Handlers,
    ) -> Self {
        let language_servers = helix_lsp::Registry::new(syn_loader.clone());
        let conf = config.load();
        // HAXX: offset the render area height by 1 to account for prompt/commandline
        area.height -= 1;

        Self {
            tabs: Vec::new(),
            active_tab: 0,
            theme: theme_loader.default(),
            language_servers,
            diagnostics: Diagnostics::new(),
            diff_providers: DiffProviderRegistry::default(),
            syn_loader,
            theme_loader,
            last_theme: None,
            registers: Registers::new(Box::new(arc_swap::access::Map::new(
                Arc::clone(&config),
                |config: &Config| &config.clipboard_provider,
            ))),
            status_msg: None,
            autoinfo: None,
            idle_timer: Box::pin(sleep(conf.idle_timeout)),
            redraw_timer: Box::pin(sleep(Duration::MAX)),
            last_motion: None,
            last_cwd: None,
            config,
            exit_code: 0,
            should_exit: false,
            config_events: unbounded_channel(),
            needs_redraw: false,
            handlers,
            dir_stack: VecDeque::with_capacity(DIR_STACK_CAP),
            layer_state: Box::new(()),
        }
    }

    pub fn popup_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Popup
    }

    pub fn menu_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Menu
    }

    pub fn apply_motion<F: Fn(&mut Self) + 'static>(&mut self, motion: F) {
        motion(self);
        self.last_motion = Some(Box::new(motion));
    }

    pub fn repeat_last_motion(&mut self, count: usize) {
        if let Some(motion) = self.last_motion.take() {
            for _ in 0..count {
                motion(self);
            }
            self.last_motion = Some(motion);
        }
    }
    /// Current editing mode for the [`Editor`].
    pub fn mode(&self) -> Mode {
        if self.tabs.is_empty() {
            return Mode::Normal;
        }
        self.tabs[self.active_tab].mode()
    }

    pub fn add_tab(&mut self, tab: Box<dyn Tab>) -> usize {
        self.tabs.push(tab);
        let index = self.tabs.len() - 1;
        self.active_tab = index;
        let doc_id = self.tabs[index].doc().id();
        self.launch_language_servers(doc_id);
        index
    }

    pub fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return self.tabs.is_empty();
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            return true;
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if self.active_tab > index {
            self.active_tab -= 1;
        }
        false
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = if self.active_tab == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab - 1
            };
        }
    }

    pub fn activate_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active_tab = index;
        }
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_index(&self) -> usize {
        self.active_tab
    }

    pub fn config(&self) -> DynGuard<Config> {
        self.config.load()
    }

    /// Call if the config has changed to let the editor update all
    /// relevant members.
    pub fn refresh_config(&mut self, old_config: &Config) {
        let config = self.config();
        self.reset_idle_timer();
        self._refresh();
        helix_event::dispatch(crate::events::ConfigDidChange {
            editor: self,
            old: old_config,
            new: &config,
        })
    }

    pub fn clear_idle_timer(&mut self) {
        // equivalent to internal Instant::far_future() (30 years)
        self.idle_timer
            .as_mut()
            .reset(Instant::now() + Duration::from_secs(86400 * 365 * 30));
    }

    pub fn reset_idle_timer(&mut self) {
        let config = self.config();
        self.idle_timer
            .as_mut()
            .reset(Instant::now() + config.idle_timeout);
    }

    pub fn clear_status(&mut self) {
        self.status_msg = None;
    }

    #[inline]
    pub fn set_status<T: Into<Cow<'static, str>>>(&mut self, status: T) {
        let status = status.into();
        log::debug!("editor status: {}", status);
        self.status_msg = Some((status, Severity::Info));
    }

    #[inline]
    pub fn set_error<T: Into<Cow<'static, str>>>(&mut self, error: T) {
        let error = error.into();
        log::debug!("editor error: {}", error);
        self.status_msg = Some((error, Severity::Error));
    }

    #[inline]
    pub fn set_warning<T: Into<Cow<'static, str>>>(&mut self, warning: T) {
        let warning = warning.into();
        log::warn!("editor warning: {}", warning);
        self.status_msg = Some((warning, Severity::Warning));
    }

    #[inline]
    pub fn get_status(&self) -> Option<(&Cow<'static, str>, &Severity)> {
        self.status_msg.as_ref().map(|(status, sev)| (status, sev))
    }

    /// Returns true if the current status is an error
    #[inline]
    pub fn is_err(&self) -> bool {
        self.status_msg
            .as_ref()
            .map(|(_, sev)| *sev == Severity::Error)
            .unwrap_or(false)
    }

    pub fn unset_theme_preview(&mut self) {
        if let Some(last_theme) = self.last_theme.take() {
            self.set_theme(last_theme);
        }
        // None likely occurs when the user types ":theme" and then exits before previewing
    }

    pub fn set_theme_preview(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Preview);
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Set);
    }

    fn set_theme_impl(&mut self, theme: Theme, preview: ThemeAction) {
        // `ui.selection` is the only scope required to be able to render a theme.
        if theme.find_highlight_exact("ui.selection").is_none() {
            self.set_error("Invalid theme: `ui.selection` required");
            return;
        }

        let scopes = theme.scopes();
        (*self.syn_loader).load().set_scopes(scopes.to_vec());

        match preview {
            ThemeAction::Preview => {
                let last_theme = std::mem::replace(&mut self.theme, theme);
                // only insert on first preview: this will be the last theme the user has saved
                self.last_theme.get_or_insert(last_theme);
            }
            ThemeAction::Set => {
                self.last_theme = None;
                self.theme = theme;
            }
        }

        self._refresh();
    }

    #[inline]
    pub fn language_server_by_id(
        &self,
        language_server_id: LanguageServerId,
    ) -> Option<&helix_lsp::Client> {
        self.language_servers
            .get_by_id(language_server_id)
            .map(|client| &**client)
    }

    /// Refreshes the language server for a given document
    pub fn refresh_language_servers(&mut self, doc_id: AppId) {
        self.launch_language_servers(doc_id)
    }

    /// moves/renames a path, invoking any event handlers (currently only lsp)
    /// and calling `set_doc_path` if the file is open in the editor
    pub fn move_path(&mut self, old_path: &Path, new_path: &Path) -> io::Result<()> {
        let new_path = helix_stdx::path::canonicalize(new_path);
        // sanity check
        if old_path == new_path {
            return Ok(());
        }
        let is_dir = old_path.is_dir();
        let language_servers: Vec<_> = self
            .language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect();
        for language_server in language_servers {
            let Some(request) = language_server.will_rename(old_path, &new_path, is_dir) else {
                continue;
            };
            let edit = match helix_lsp::block_on(request) {
                Ok(edit) => edit.unwrap_or_default(),
                Err(err) => {
                    log::error!("invalid willRename response: {err:?}");
                    continue;
                }
            };
            if let Err(err) = self.apply_workspace_edit(language_server.offset_encoding(), &edit) {
                log::error!("failed to apply workspace edit: {err:?}")
            }
        }

        if old_path.exists() {
            fs::rename(old_path, &new_path)?;
        }

        // Check if the current doc matches the old path
        let matches = self.tabs[self.active_tab].doc().path().map(|p| p == old_path).unwrap_or(false);
        if matches {
            let doc_id = self.tabs[self.active_tab].doc().id();
            self.set_doc_path(doc_id, &new_path);
        }
        let is_dir = new_path.is_dir();
        for ls in self.language_servers.iter_clients() {
            // A new language server might have been started in `set_doc_path` and won't
            // be initialized yet. Skip the `did_rename` notification for this server.
            if !ls.is_initialized() {
                continue;
            }
            ls.did_rename(old_path, &new_path, is_dir);
        }
        self.language_servers
            .file_event_handler
            .file_changed(old_path.to_owned());
        self.language_servers
            .file_event_handler
            .file_changed(new_path);
        Ok(())
    }

    pub fn set_doc_path(&mut self, doc_id: AppId, path: &Path) {
        let doc = self.tabs[self.active_tab].doc_mut();
        let old_path = doc.path();

        if let Some(old_path) = old_path {
            // sanity check, should not occur but some callers (like an LSP) may
            // create bogus calls
            if old_path == path {
                return;
            }
            // if we are open in LSPs send did_close notification
            for language_server in doc.language_servers() {
                language_server.text_document_did_close(doc.identifier());
            }
        }
        // we need to clear the list of language servers here so that
        // refresh_doc_language/refresh_language_servers doesn't resend
        // text_document_did_close. Since we called `text_document_did_close`
        // we have fully unregistered this document from its LS
        doc.language_servers.clear();
        doc.set_path(Some(path));
        doc.detect_editor_config();
        self.refresh_doc_language(doc_id)
    }

    pub fn refresh_doc_language(&mut self, doc_id: AppId) {
        let loader = self.syn_loader.load();
        let doc = self.tabs[self.active_tab].doc_mut();
        doc.detect_language(&loader);
        doc.detect_editor_config();
        doc.detect_indent_and_line_ending();
        self.refresh_language_servers(doc_id);
        let doc = self.tabs[self.active_tab].doc_mut();
        let diagnostics = Editor::doc_diagnostics(&self.language_servers, &self.diagnostics, doc);
        doc.replace_diagnostics(diagnostics, &[], None);
        doc.reset_all_inlay_hints();
    }

    /// Launch a language server for a given document
    pub fn launch_language_servers(&mut self, _doc_id: AppId) {
        if !self.config().lsp.enable {
            return;
        }
        // if doc doesn't have a URL it's a scratch buffer, ignore it
        let doc = self.tabs[self.active_tab].doc_mut();
        let Some(doc_url) = doc.url() else {
            return;
        };
        let (lang, path) = (doc.language.clone(), doc.path().cloned());
        let config = doc.config.load();
        let root_dirs = &config.workspace_lsp_roots;

        // store only successfully started language servers
        let language_servers = lang.as_ref().map_or_else(HashMap::default, |language| {
            self.language_servers
                .get(language, path.as_ref(), root_dirs, false)
                .filter_map(|(lang, client)| match client {
                    Ok(client) => Some((lang, client)),
                    Err(err) => {
                        if let helix_lsp::Error::ExecutableNotFound(err) = err {
                            // Silence by default since some language servers might just not be installed
                            log::debug!(
                                "Language server not found for `{}` {} {}", language.scope, lang, err,
                            );
                        } else {
                            log::error!(
                                "Failed to initialize the language servers for `{}` - `{}` {{ {} }}",
                                language.scope,
                                lang,
                                err
                            );
                        }
                        None
                    }
                })
                .collect::<HashMap<_, _>>()
        });

        if language_servers.is_empty() && doc.language_servers.is_empty() {
            return;
        }

        let language_id = doc.language_id().map(ToOwned::to_owned).unwrap_or_default();

        // only spawn new language servers if the servers aren't the same
        let doc_language_servers_not_in_registry =
            doc.language_servers.iter().filter(|(name, doc_ls)| {
                language_servers
                    .get(*name)
                    .is_none_or(|ls| ls.id() != doc_ls.id())
            });

        for (_, language_server) in doc_language_servers_not_in_registry {
            language_server.text_document_did_close(doc.identifier());
        }

        let language_servers_not_in_doc = language_servers.iter().filter(|(name, ls)| {
            doc.language_servers
                .get(*name)
                .is_none_or(|doc_ls| ls.id() != doc_ls.id())
        });

        for (_, language_server) in language_servers_not_in_doc {
            // TODO: this now races with on_init code if the init happens too quickly
            language_server.text_document_did_open(
                doc_url.clone(),
                doc.version(),
                doc.text(),
                language_id.clone(),
            );
        }

        doc.language_servers = language_servers;
    }

    /// Close a view by removing it from the tree. If the tree becomes empty,
    /// remove the tab entirely.
    pub fn close(&mut self, view_id: ViewId) {
        if self.tabs.is_empty() {
            return;
        }
        self.tabs[self.active_tab].tree_mut().remove(view_id);
        // If tree has no views left, remove the tab
        if self.tabs[self.active_tab].tree().views().count() == 0 {
            self.close_tab(self.active_tab);
        } else {
            self._refresh();
        }
    }

    fn _refresh(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let config = self.config();
        let tab = &mut self.tabs[self.active_tab];

        if !config.lsp.display_inlay_hints {
            tab.doc_mut().reset_all_inlay_hints();
        }

        let (doc, tree) = tab.doc_and_tree_mut();
        for (view, _) in tree.views_mut() {
            view.sync_changes(doc);
            view.gutters = config.gutters.clone();
            view.ensure_cursor_in_view(doc, config.scrolloff)
        }
    }

    pub fn resize(&mut self, area: Rect) {
        if self.tabs.is_empty() {
            return;
        }
        if self.tabs[self.active_tab].tree_mut().resize(area) {
            self._refresh();
        };
    }

    pub fn focus(&mut self, view_id: ViewId) {
        if self.tabs.is_empty() {
            return;
        }
        if self.tabs[self.active_tab].tree().focus == view_id {
            return;
        }

        // Reset mode to normal and ensure any pending changes are committed in the old document.
        self.enter_normal_mode();
        {
            let tab = &mut self.tabs[self.active_tab];
            let focus = tab.tree().focus;
            let (doc, tree) = tab.doc_and_tree_mut();
            let view = tree.get_mut(focus);
            doc.append_changes_to_history(view);
        }
        self.ensure_cursor_in_view(view_id);
        // Update jumplist selections with new document changes.
        {
            let (doc, tree) = self.tabs[self.active_tab].doc_and_tree_mut();
            for (view, _focused) in tree.views_mut() {
                view.sync_changes(doc);
            }
        }

        let tab = &mut self.tabs[self.active_tab];
        let prev_id = std::mem::replace(&mut tab.tree_mut().focus, view_id);
        tab.doc_mut().mark_as_focused();

        let focus_lost = self.tabs[self.active_tab].tree().get(prev_id).doc;
        dispatch(DocumentFocusLost {
            editor: self,
            doc: focus_lost,
        });
    }

    pub fn focus_next(&mut self) {
        let next = self.tabs[self.active_tab].tree().next();
        self.focus(next);
    }

    pub fn focus_prev(&mut self) {
        let prev = self.tabs[self.active_tab].tree().prev();
        self.focus(prev);
    }

    pub fn focus_direction(&mut self, direction: tree::Direction) {
        let current_view = self.tabs[self.active_tab].tree().focus;
        if let Some(id) = self.tabs[self.active_tab].tree().find_split_in_direction(current_view, direction) {
            self.focus(id)
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: tree::Direction) {
        self.tabs[self.active_tab].tree_mut().swap_split_in_direction(direction);
    }

    pub fn transpose_view(&mut self) {
        self.tabs[self.active_tab].tree_mut().transpose();
    }

    pub fn should_close(&self) -> bool {
        self.should_exit
    }

    pub fn ensure_cursor_in_view(&mut self, id: ViewId) {
        if self.tabs.is_empty() {
            return;
        }
        let config = self.config();
        let (doc, tree) = self.tabs[self.active_tab].doc_and_tree_mut();
        let view = tree.get(id);
        view.ensure_cursor_in_view(doc, config.scrolloff)
    }

    /// Returns all supported diagnostics for the document
    pub fn doc_diagnostics<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        Editor::doc_diagnostics_with_filter(language_servers, diagnostics, document, |_, _| true)
    }

    /// Returns all supported diagnostics for the document
    /// filtered by `filter` which is invocated with the raw `lsp::Diagnostic` and the language server id it came from
    pub fn doc_diagnostics_with_filter<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
        filter: impl Fn(&lsp::Diagnostic, &DiagnosticProvider) -> bool + 'a,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        let text = document.text().clone();
        let language_config = document.language.clone();
        document
            .uri()
            .and_then(|uri| diagnostics.get(&uri))
            .map(|diags| {
                diags.iter().filter_map(move |(diagnostic, provider)| {
                    let server_id = provider.language_server_id()?;
                    let ls = language_servers.get_by_id(server_id)?;
                    language_config
                        .as_ref()
                        .and_then(|c| {
                            c.language_servers.iter().find(|features| {
                                features.name == ls.name()
                                    && features.has_feature(LanguageServerFeature::Diagnostics)
                            })
                        })
                        .and_then(|_| {
                            if filter(diagnostic, provider) {
                                Document::lsp_diagnostic_to_diagnostic(
                                    &text,
                                    language_config.as_deref(),
                                    diagnostic,
                                    provider.clone(),
                                    ls.offset_encoding(),
                                )
                            } else {
                                None
                            }
                        })
                })
            })
            .into_iter()
            .flatten()
    }

    /// Gets the primary cursor position in screen coordinates,
    /// or `None` if the primary cursor is not visible on screen.
    pub fn cursor(&self) -> (Option<Position>, CursorKind) {
        if self.tabs.is_empty() {
            return (None, CursorKind::default());
        }
        let config = self.config();
        let tab = &self.tabs[self.active_tab];
        let (view, doc) = current_ref!(self);
        if let Some(mut pos) = tab.cursor_cache().get(view, doc) {
            let inner = view.inner_area(doc);
            pos.col += inner.x as usize;
            pos.row += inner.y as usize;
            let cursorkind = config.cursor_shape.from_mode(tab.mode());
            (Some(pos), cursorkind)
        } else {
            (None, CursorKind::default())
        }
    }

    /// Closes language servers with timeout. The default timeout is 10000 ms, use
    /// `timeout` parameter to override this.
    pub async fn close_language_servers(
        &self,
        timeout: Option<u64>,
    ) -> Result<(), tokio::time::error::Elapsed> {
        // Remove all language servers from the file event handler.
        // Note: this is non-blocking.
        for client in self.language_servers.iter_clients() {
            self.language_servers
                .file_event_handler
                .remove_client(client.id());
        }

        tokio::time::timeout(
            Duration::from_millis(timeout.unwrap_or(3000)),
            future::join_all(
                self.language_servers
                    .iter_clients()
                    .map(|client| client.force_shutdown()),
            ),
        )
        .await
        .map(|_| ())
    }

    pub async fn wait_event(&mut self) -> EditorEvent {
        // the loop only runs once or twice and would be better implemented with a recursion + const generic
        // however due to limitations with async functions that can not be implemented right now
        loop {
            tokio::select! {
                biased;

                Some(config_event) = self.config_events.1.recv() => {
                    return EditorEvent::ConfigEvent(config_event)
                }
                Some(message) = self.language_servers.incoming.next() => {
                    return EditorEvent::LanguageServerMessage(message)
                }
                _ = helix_event::redraw_requested() => {
                    if  !self.needs_redraw{
                        self.needs_redraw = true;
                        let timeout = Instant::now() + Duration::from_millis(33);
                        if timeout < self.idle_timer.deadline() && timeout < self.redraw_timer.deadline(){
                            self.redraw_timer.as_mut().reset(timeout)
                        }
                    }
                }

                _ = &mut self.redraw_timer  => {
                    self.redraw_timer.as_mut().reset(Instant::now() + Duration::from_secs(86400 * 365 * 30));
                    return EditorEvent::Redraw
                }
                _ = &mut self.idle_timer  => {
                    return EditorEvent::IdleTimer
                }
            }
        }
    }

    /// Switches the editor into normal mode.
    pub fn enter_normal_mode(&mut self) {
        use helix_core::graphemes;

        if self.tabs[self.active_tab].mode() == Mode::Normal {
            return;
        }

        self.tabs[self.active_tab].set_mode(Mode::Normal);
        let (view, doc) = current!(self);

        try_restore_indent(doc, view);

        // if leaving append mode, move cursor back by 1
        if doc.restore_cursor {
            let text = doc.text().slice(..);
            let selection = doc.selection(view.id).clone().transform(|range| {
                let mut head = range.to();
                if range.head > range.anchor {
                    head = graphemes::prev_grapheme_boundary(text, head);
                }

                Range::new(range.from(), head)
            });

            doc.set_selection(view.id, selection);
            doc.restore_cursor = false;
        }
    }

    /// Returns the id of a view that this doc contains a selection for,
    /// making sure it is synced with the current changes.
    /// If possible or there are no selections returns current_view,
    /// otherwise uses an arbitrary view.
    pub fn get_synced_view_id(&mut self, _id: AppId) -> ViewId {
        let tab = &mut self.tabs[self.active_tab];
        let focus = tab.tree().focus;
        let (doc, tree) = tab.doc_and_tree_mut();
        let current_view = tree.get_mut(focus);
        if doc.selections().contains_key(&current_view.id) {
            // only need to sync current view if this is not the current doc
            if current_view.doc != _id {
                current_view.sync_changes(doc);
            }
            current_view.id
        } else if let Some(view_id) = doc.selections().keys().next() {
            let view_id = *view_id;
            let view = tree.get_mut(view_id);
            view.sync_changes(doc);
            view_id
        } else {
            doc.ensure_view_init(current_view.id);
            current_view.id
        }
    }

    pub fn set_cwd(&mut self, path: &Path) -> std::io::Result<()> {
        self.last_cwd = helix_stdx::env::set_current_working_dir(path)?;
        self.clear_doc_relative_paths();
        Ok(())
    }

    pub fn get_last_cwd(&mut self) -> Option<&Path> {
        self.last_cwd.as_deref()
    }

    pub fn jump_forward(&mut self, view_id: ViewId, count: usize) {
        if let Some((doc_id, selection)) = self.tabs[self.active_tab].tree_mut().get_mut(view_id).jumps.forward(count).cloned() {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    pub fn jump_backward(&mut self, view_id: ViewId, count: usize) {
        let (doc, tree) = self.tabs[self.active_tab].doc_and_tree_mut();
        let view = tree.get_mut(view_id);
        if let Some((doc_id, selection)) = view
            .jumps
            .backward(view_id, doc, count)
            .cloned()
        {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    fn jump_to(&mut self, view_id: ViewId, _dest_doc_id: AppId, mut selection: Selection) {
        {
            let (doc, tree) = self.tabs[self.active_tab].doc_and_tree_mut();
            let view = tree.get_mut(view_id);
            if let Some(transaction) = view.changes_to_sync(doc) {
                let text = doc.text().slice(..);
                selection = selection.map(transaction.changes()).ensure_invariants(text);
            }
        }
        let tab = &mut self.tabs[self.active_tab];
        let focus = tab.tree().focus;
        let (doc, tree) = tab.doc_and_tree_mut();
        let view = tree.get_mut(focus);
        doc.set_selection(view_id, selection);
        view.ensure_cursor_in_view_center(doc, self.config.load().scrolloff);
    }
}

fn try_restore_indent(doc: &mut Document, view: &mut View) {
    use helix_core::{
        chars::char_is_whitespace,
        line_ending::{line_end_char_index, str_is_line_ending},
        unicode::segmentation::UnicodeSegmentation,
        Operation, Transaction,
    };

    fn inserted_a_new_blank_line(changes: &[Operation], pos: usize, line_end_pos: usize) -> bool {
        if let [Operation::Retain(move_pos), Operation::Insert(ref inserted_str), Operation::Retain(_)] =
            changes
        {
            let mut graphemes = inserted_str.graphemes(true);
            move_pos + inserted_str.len() == pos
                && graphemes.next().is_some_and(str_is_line_ending)
                && graphemes.all(|g| g.chars().all(char_is_whitespace))
                && pos == line_end_pos // ensure no characters exists after current position
        } else {
            false
        }
    }

    let doc_changes = doc.changes().changes();
    let text = doc.text().slice(..);
    let range = doc.selection(view.id).primary();
    let pos = range.cursor(text);
    let line_end_pos = line_end_char_index(&text, range.cursor_line(text));

    if inserted_a_new_blank_line(doc_changes, pos, line_end_pos) {
        // Removes tailing whitespaces for the primary selection only, preserving existing behavior
        let line_start_pos = text.line_to_char(range.cursor_line(text));
        let transaction =
            Transaction::change(doc.text(), [(line_start_pos, pos, None)].into_iter());
        doc.apply(&transaction, view.id);
    }
}

#[derive(Default)]
pub struct CursorCache(Cell<Option<Option<Position>>>);

impl CursorCache {
    pub fn get(&self, view: &View, doc: &Document) -> Option<Position> {
        if let Some(pos) = self.0.get() {
            return pos;
        }

        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let res = view.screen_coords_at_pos(doc, text, cursor);
        self.set(res);
        res
    }

    pub fn set(&self, cursor_pos: Option<Position>) {
        self.0.set(Some(cursor_pos))
    }

    pub fn reset(&self) {
        self.0.set(None)
    }
}
