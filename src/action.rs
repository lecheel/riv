//! Editor action definitions.

use std::path::PathBuf;

/// Semantic category for grouping actions in help displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionCategory {
    Movement,
    Scrolling,
    Editing,
    Insert,
    Mode,
    Search,
    YankPaste,
    Undo,
    File,
    Window,
    Git,
    Completion,
    Llm,
    Visual,
    Ripgrep,
    Buffer,
    RegisterPrefix,
    FunctionList,
    Marks,
    Misc,
    Tags,
}

impl ActionCategory {
    /// Display header for this category in the help popup.
    pub fn header(&self) -> &'static str {
        match self {
            ActionCategory::Movement => "Movement",
            ActionCategory::Scrolling => "Scrolling",
            ActionCategory::Editing => "Editing",
            ActionCategory::Insert => "Insert",
            ActionCategory::Mode => "Mode",
            ActionCategory::Search => "Search",
            ActionCategory::YankPaste => "Yank / Paste",
            ActionCategory::Undo => "Undo / Redo",
            ActionCategory::File => "File",
            ActionCategory::Window => "Window",
            ActionCategory::Git => "Git",
            ActionCategory::Completion => "Completion",
            ActionCategory::Llm => "LLM",
            ActionCategory::Visual => "Visual",
            ActionCategory::Ripgrep => "Ripgrep",
            ActionCategory::Buffer => "Buffers",
            ActionCategory::Marks => "Marks",
            ActionCategory::RegisterPrefix => "Registers",
            ActionCategory::Misc => "Misc",
            ActionCategory::Tags => "Tags",
            ActionCategory::FunctionList => "Functions",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // ── Movement ───────────────────────────────────────────
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordForward,
    MoveWordBack,
    MoveWordEnd,
    MoveLineStart,
    MoveLineEnd,
    MoveFileStart,
    MoveFileEnd,
    MoveToLine(usize),
    MoveToPosition { line: usize, col: usize },

    // ── Scrolling ──────────────────────────────────────────
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
    PageUp,
    PageDown,
    ScrollCenter,

    // ── Insert / Editing ───────────────────────────────────
    InsertChar(char),
    InsertNewline,
    InsertTab,
    InsertText(String),
    InsertRegisterPrefix,
    DeleteChar,
    DeleteCharForward,
    DeleteWord,
    DeleteWordForward,
    DeleteLine,
    DeleteToLineEnd,
    DeleteToFileEnd,
    DeleteToLineStart,
    DeleteSelection,
    ChangeSelection,
    ReplaceChar,
    Backspace,
    OpenLineBelow,
    OpenLineAbove,
    JoinLines,
    Indent,
    Dedent,
    IndentSelection,
    DedentSelection,
    ToggleComment,
    ToggleCommentAndMoveDown,
    DeleteAroundFunction,
    IndentTs,
    IndentTsToFileEnd,
    Register,

    // ── Visual mode block operations ────────────────────
    BlockInsert,
    BlockAppend,
    SwapSelectionAnchor,

    // ── Mode changes ───────────────────────────────────────
    EnterNormalMode,
    EnterInsertMode,
    EnterAppendMode,
    EnterInsertLineStart,
    EnterAppendLineEnd,
    EnterReplaceMode,
    EnterVisualMode,
    EnterVisualLineMode,
    EnterVisualBlockMode,
    EnterCommandMode,
    EnterOperatorPending(String),
    EnterJumpMode,

    // ── Yank / Paste ───────────────────────────────────────
    YankSelection,
    YankLine,
    PasteAfter,
    PasteBefore,
    YankToClipboard,
    PasteFromClipboard,
    ClipboardPasteLine,
    ClipboardReplaceBuffer,

    // ── Undo / Redo ────────────────────────────────────────
    Undo,
    Redo,
    UndoBreak,

    // ── Search ─────────────────────────────────────────────
    SearchForward,
    SearchBackward,
    SearchNext,
    SearchPrev,
    SearchWordForward,
    SearchWordBackward,
    ReplaceMode,
    ReplaceAll,

    // ── Tags (ctags) ──────────────────────────────────────
    TagJump,
    TagNext,
    TagPrev,
    TagPop,
    GenerateTags,

    // ── File operations ────────────────────────────────────
    Save,
    SaveFmt,
    SaveAs(PathBuf),
    OpenFile(PathBuf),
    NewFile,
    CloseFile,
    FindFile,
    DeleteBuffer,

    // ── Window management ──────────────────────────────────
    SplitHorizontal,
    SplitVertical,
    NextWindow,
    PrevWindow,
    CloseWindow,
    ZoomWindow,
    EqualizeWindows,
    SwapWindowLeft,
    SwapWindowRight,

    // ── Command line ───────────────────────────────────────
    ExecuteCommand,
    CommandHistoryUp,
    CommandHistoryDown,

    // ── LSP ────────────────────────────────────────────────
    GotoDefinition,
    GotoDeclaration,
    GotoImplementation,
    GotoTypeDefinition,
    FindReferences,
    RenameSymbol,
    HoverInfo,
    CodeAction,
    FormatDocument,
    SignatureHelp,
    Diagnostics,

    // ── Git ────────────────────────────────────────────────
    GitStatus,
    GitDiff,
    GitStageHunk,
    GitUnstageHunk,
    GitBlame,
    GitLog,
    GitNextHunk,
    GitPrevHunk,
    GitRevertHunk,
    GitGutterToggle,
    GitCommit,

    // ── Completion ─────────────────────────────────────────
    TriggerCompletion,
    SelectNextCompletion,
    SelectPrevCompletion,
    ConfirmCompletion,
    CancelCompletion,

    // ── Bracket matching ───────────────────────────────────
    MatchBracket,

    RipgrepUnderCursor,
    RipgrepInput,
    RipgrepGotoResult,
    RipgrepClose,
    RipgrepLast,
    RipgrepNextResult,
    RipgrepPrevResult,

    ListBuffers,
    NextBuffer,
    PrevBuffer,
    OpenMru,

    LlmOpen,
    LlmClose,
    LlmSend,
    LlmCancel,
    LlmClearHistory,
    LlmNextPreset,
    LlmPrevPreset,
    LlmEnterPrompt,
    LlmQuickPrompt,
    LlmQuickCheckEnglish,
    LlmQuickTranslateChinese,
    LlmQuickTranslateEnglish,
    LlmQuickExplain,
    LlmQuickSummarize,
    LlmSessionNew,
    TriggerCodeiumCompletion,

    // ── Marks / Bookmarks ──────────────────────────────────
    SetMark,
    GotoMark,
    JumpBack,
    RegisterPrefix,
    FunctionList,

    ShowShortcuts,
    Quit,
    ForceQuit,
    ShowHelp,
    ToggleLineNumbers,
    ToggleWhitespace,
    ClearMessages,
    RepeatLastAction,
    RunBuild,
    None,
}

impl Action {
    /// Return the semantic category for grouping in help displays.
    pub fn category(&self) -> ActionCategory {
        match self {
            // Movement
            Self::MoveLeft
            | Self::MoveRight
            | Self::MoveUp
            | Self::MoveDown
            | Self::MoveWordForward
            | Self::MoveWordBack
            | Self::MoveWordEnd
            | Self::MoveLineStart
            | Self::MoveLineEnd
            | Self::MoveFileStart
            | Self::MoveFileEnd
            | Self::MoveToLine(_)
            | Self::MoveToPosition { .. }
            | Self::EnterJumpMode
            | Self::MatchBracket => ActionCategory::Movement,

            // Scrolling
            Self::ScrollUp
            | Self::ScrollDown
            | Self::ScrollLeft
            | Self::ScrollRight
            | Self::PageUp
            | Self::PageDown
            | Self::ScrollCenter => ActionCategory::Scrolling,

            // Editing
            Self::DeleteChar
            | Self::DeleteCharForward
            | Self::DeleteWord
            | Self::DeleteWordForward
            | Self::DeleteLine
            | Self::DeleteToLineEnd
            | Self::DeleteToFileEnd
            | Self::DeleteToLineStart
            | Self::DeleteSelection
            | Self::ChangeSelection
            | Self::ReplaceChar
            | Self::Backspace
            | Self::JoinLines
            | Self::Indent
            | Self::Dedent
            | Self::IndentSelection
            | Self::DedentSelection
            | Self::ToggleComment
            | Self::ToggleCommentAndMoveDown
            | Self::DeleteAroundFunction
            | Self::IndentTs
            | Self::IndentTsToFileEnd
            | Self::BlockInsert
            | Self::BlockAppend
            | Self::Register
            | Self::SwapSelectionAnchor => ActionCategory::Editing,

            // Insert
            Self::InsertChar(_)
            | Self::InsertNewline
            | Self::InsertTab
            | Self::InsertText(_)
            | Self::InsertRegisterPrefix
            | Self::OpenLineBelow
            | Self::OpenLineAbove => ActionCategory::Insert,

            // Mode
            Self::EnterNormalMode
            | Self::EnterInsertMode
            | Self::EnterAppendMode
            | Self::EnterInsertLineStart
            | Self::EnterAppendLineEnd
            | Self::EnterReplaceMode
            | Self::EnterVisualMode
            | Self::EnterVisualLineMode
            | Self::EnterVisualBlockMode
            | Self::EnterCommandMode
            | Self::EnterOperatorPending(_) => ActionCategory::Mode,

            // Search
            Self::SearchForward
            | Self::SearchBackward
            | Self::SearchNext
            | Self::SearchPrev
            | Self::SearchWordForward
            | Self::SearchWordBackward
            | Self::ReplaceMode
            | Self::ReplaceAll => ActionCategory::Search,

            // Yank / Paste
            Self::YankSelection
            | Self::YankLine
            | Self::PasteAfter
            | Self::PasteBefore
            | Self::YankToClipboard
            | Self::PasteFromClipboard
            | Self::ClipboardPasteLine
            | Self::ClipboardReplaceBuffer => ActionCategory::YankPaste,

            // Undo / Redo
            Self::Undo | Self::Redo | Self::UndoBreak => ActionCategory::Undo,

            // Tags
            Self::TagJump | Self::TagNext | Self::TagPrev | Self::TagPop | Self::GenerateTags => {
                ActionCategory::Marks
            }

            // File
            Self::Save
            | Self::SaveFmt
            | Self::SaveAs(_)
            | Self::OpenFile(_)
            | Self::NewFile
            | Self::CloseFile
            | Self::FindFile => ActionCategory::File,

            // Window
            Self::SplitHorizontal
            | Self::SplitVertical
            | Self::NextWindow
            | Self::PrevWindow
            | Self::CloseWindow
            | Self::ZoomWindow
            | Self::EqualizeWindows
            | Self::SwapWindowLeft
            | Self::SwapWindowRight => ActionCategory::Window,

            // Git
            Self::GitStatus
            | Self::GitDiff
            | Self::GitStageHunk
            | Self::GitUnstageHunk
            | Self::GitBlame
            | Self::GitLog
            | Self::GitNextHunk
            | Self::GitPrevHunk
            | Self::GitRevertHunk
            | Self::GitCommit
            | Self::GitGutterToggle => ActionCategory::Git,

            // Completion
            Self::TriggerCompletion
            | Self::SelectNextCompletion
            | Self::SelectPrevCompletion
            | Self::ConfirmCompletion
            | Self::CancelCompletion
            | Self::TriggerCodeiumCompletion => ActionCategory::Completion,

            // LLM
            Self::LlmOpen
            | Self::LlmClose
            | Self::LlmSend
            | Self::LlmCancel
            | Self::LlmClearHistory
            | Self::LlmNextPreset
            | Self::LlmPrevPreset
            | Self::LlmEnterPrompt
            | Self::LlmQuickPrompt
            | Self::LlmQuickCheckEnglish
            | Self::LlmQuickTranslateChinese
            | Self::LlmQuickTranslateEnglish
            | Self::LlmQuickExplain
            | Self::LlmQuickSummarize
            | Self::LlmSessionNew => ActionCategory::Llm,

            // Ripgrep
            Self::RipgrepUnderCursor
            | Self::RipgrepInput
            | Self::RipgrepGotoResult
            | Self::RipgrepClose
            | Self::RipgrepLast
            | Self::RipgrepNextResult
            | Self::RipgrepPrevResult => ActionCategory::Ripgrep,

            // Buffer
            Self::ListBuffers
            | Self::NextBuffer
            | Self::PrevBuffer
            | Self::DeleteBuffer
            | Self::OpenMru => ActionCategory::Buffer,

            // LSP
            Self::GotoDefinition
            | Self::GotoDeclaration
            | Self::GotoImplementation
            | Self::GotoTypeDefinition
            | Self::FindReferences
            | Self::RenameSymbol
            | Self::HoverInfo
            | Self::CodeAction
            | Self::FormatDocument
            | Self::SignatureHelp
            | Self::Diagnostics => ActionCategory::Misc,

            // Marks / Bookmarks
            Self::SetMark | Self::GotoMark | Self::JumpBack => ActionCategory::Marks,
            Self::RegisterPrefix => ActionCategory::RegisterPrefix,
            Self::FunctionList => ActionCategory::FunctionList,

            // Misc
            Self::ExecuteCommand
            | Self::CommandHistoryUp
            | Self::CommandHistoryDown
            | Self::Quit
            | Self::ForceQuit
            | Self::ShowHelp
            | Self::ToggleLineNumbers
            | Self::ToggleWhitespace
            | Self::ClearMessages
            | Self::RepeatLastAction
            | Self::ShowShortcuts
            | Self::RunBuild
            | Self::None => ActionCategory::Misc,
        }
    }

    /// Return a short snake_case label for the action.
    pub fn label(&self) -> String {
        let debug_str = format!("{:?}", self);
        let variant_name = if let Some(pos) = debug_str.find('(') {
            &debug_str[..pos]
        } else if let Some(pos) = debug_str.find(" {") {
            &debug_str[..pos]
        } else {
            &debug_str
        };
        camel_to_snake(variant_name)
    }
}

pub fn camel_to_snake(name: &str) -> String {
    let mut result = String::with_capacity(name.len() + 8);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
        } else {
            result.push(ch);
        }
    }
    result
}
