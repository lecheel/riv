# LSP Design Specification

## 1. Overview

The LSP (Language Server Protocol) subsystem provides non‑blocking integration with language servers, enabling features such as diagnostics, code completion, goto definition, formatting, inlay hints, and signature help. The implementation is fully asynchronous, built on `tokio`, and integrates with the editor’s event‑loop via message passing.

Key design goals:
- **Non‑blocking**: Editor UI never waits for LSP responses.
- **Request tracking**: Pending requests are stored with associated completion channels.
- **Lifecycle management**: LSP server is started on‑demand per language and can be restarted.
- **Protocol completeness**: Supports `initialize`, `textDocument/*` notifications, and server‑push diagnostics.
- **Encoding awareness**: Handles UTF‑8, UTF‑16, and UTF‑32 position encodings.

## 2. Architecture

The LSP subsystem consists of three main layers:

```
┌─────────────────────────────────────────────────────────┐
│                      Editor Main Loop                   │
│  (handles input, rendering, editing commands)           │
└─────────────────────────────────────────────────────────┘
                            │
                    AppMessage / LspMessage
                            ▼
┌─────────────────────────────────────────────────────────┐
│                      LspManager                         │
│  • runs in its own tokio task                           │
│  • owns the active LanguageServer (if any)              │
│  • maintains opened files, versions, pending requests   │
│  • dispatches editor messages to the LSP server         │
│  • forwards server responses back via AppSender         │
└─────────────────────────────────────────────────────────┘
                            │
                    ┌───────┴───────┐
                    ▼               ▼
         LanguageServer (lsp.rs)   background reader task
         • sends JSON‑RPC requests  • reads stdout
         • assigns request IDs       • parses Content‑Length
         • tracks capabilities       • forwards messages to LspManager
```

### 2.1 Message Flow

1. **Editor → LspManager**: Editor sends `LspMessage` commands (open file, change, request completions, etc.) via an unbounded `mpsc` channel.
2. **LspManager → LanguageServer**: `LspManager` translates editor messages into LSP requests/notifications and sends them over the server’s `stdin`.
3. **LanguageServer → background reader**: A separate tokio task reads the server’s `stdout`, parses the LSP message framing (`Content-Length` headers), and forwards decoded JSON messages back to `LspManager` via another `mpsc` channel.
4. **LspManager → Editor**: Responses are matched with pending request IDs, transformed into `AppMessage` variants, and sent back to the editor’s main loop.

## 3. LSP Protocol Types

The module defines the essential LSP data structures as Rust types (using `serde`). Key types include:

| Type | Purpose |
|------|---------|
| `InitializeParams` / `ClientCapabilities` | Server initialization |
| `TextDocumentItem`, `DidOpenTextDocumentParams` | Opening a file |
| `DidChangeTextDocumentParams` (full or incremental) | Content changes |
| `PublishDiagnosticsParams` | Server‑pushed diagnostics |
| `CompletionItem`, `CompletionList`, `CompletionResponse` | Autocompletion |
| `InlayHintParams`, `InlayHint` | Inlay hints display |
| `SignatureHelpParams`, `SignatureHelp` | Function signature popups |
| `DocumentFormattingParams`, `TextEdit` | Code formatting |
| `Location`, `LocationLink` | Goto definition results |

### 3.1 Offset Encoding

To convert between character indices (used internally by the rope data structure) and LSP `Position.line` / `character`, the system supports three encodings:

- `OffsetEncoding::Utf8` – positions are byte offsets in the UTF‑8 encoded line.
- `OffsetEncoding::Utf16` – default per LSP spec; positions are UTF‑16 code unit offsets.
- `OffsetEncoding::Utf32` – positions are Unicode scalar value offsets.

Helper functions `pos_to_lsp_pos()` and `lsp_pos_to_char_idx()` perform the conversions using the selected encoding obtained from server capabilities.

## 4. LanguageServer Implementation

`LanguageServer` is the low‑level client that communicates with one specific language server process.

### 4.1 Initialization

`LanguageServer::new()` spawns the server process, sets up pipes, and performs the blocking `initialize` handshake. It then sends an `initialized` notification. During this phase, it captures server capabilities:

- `offset_encoding`: the position encoding supported by the server.
- `supports_snippets`: whether `CompletionItem.insertTextFormat = 2` (snippet) is allowed.
- `supports_completion_resolve`: whether the server supports `completionItem/resolve`.

### 4.2 Request Handling

- `send_request()` assigns a new integer ID (incremented) and writes a JSON‑RPC request to `stdin`. Returns the ID immediately, without waiting.
- `notify()` sends a fire‑and‑forget notification (no ID).
- `blocking_request()` is only used during `initialize`; it waits for the response on the receiver channel using a timeout.

### 4.3 Change Notifications

The server exposes convenience methods:
- `did_open()` – sends `textDocument/didOpen` with initial content.
- `did_change()` – sends a full‑text change (no range).
- `did_change_incremental()` – sends a change with a `range` for incremental sync.
- `did_save()` – sends `textDocument/didSave`.

### 4.4 Shutdown

`shutdown()` sends a `shutdown` request followed by an `exit` notification, then kills the process and waits for it to exit.

## 5. LspManager

`LspManager` is the central orchestrator, running a `select!` loop that alternates between incoming editor messages and server responses.

### 5.1 State

```rust
pub struct LspManager {
    pub active_lsp: Option<LanguageServer>,          // current LSP instance
    lsp_rx: mpsc::UnboundedReceiver<LspMessage>,    // from editor
    lsp_tx: mpsc::UnboundedSender<LspMessage>,      // cloneable for external use
    app_tx: AppSender,                              // back to editor
    opened_files: HashSet<String>,                  // URIs opened via didOpen
    current_file_version: HashMap<String, i32>,     // version per document
    pending_hint_requests: HashMap<String, i32>,    // dedup inlay hints
    pending: HashMap<i32, PendingKind>,             // request ID → callback info
    pub supports_snippets: bool,
    pub supports_completion_resolve: bool,
}
```

### 5.2 Pending Request Kinds

Each pending request is stored with enough context to deliver the response to the correct editor subsystem.

```rust
enum PendingKind {
    GotoDefinition(oneshot::Sender<Vec<Location>>),
    Formatting {
        buffer_idx: usize,
        cursor_state: Option<(usize, usize)>,
        save_after: bool,
    },
    InlayHints { uri: String, version: i32 },
    SignatureHelp,
    Completion,
    ResolveCompletion,
    Shutdown,
    Ignore,
}
```

### 5.3 Main Loop

The loop runs continuously. When an LSP server is active, it uses `tokio::select!` to wait for either:

- An editor message (`lsp_rx.recv()`) → `dispatch_editor_msg()`
- A server message (`lsp.rx.recv()`) → `handle_server_msg()`

If the server dies (channel closed), all pending requests are rejected, and `active_lsp` is set to `None`.

When no server is active, only editor messages are processed; they may trigger a lazy startup (`start_lsp_for_file`).

### 5.4 Message Dispatching

`dispatch_editor_msg()` handles each `LspMessage` variant:

| Message | Action |
|---------|--------|
| `OpenFile` | Starts LSP if needed, sends `didOpen` |
| `CloseFile` | Sends `didClose` and removes from `opened_files` |
| `ChangeFileIncremental` | Sends incremental `didChange`, clears pending diagnostics optimistically |
| `ChangeFile` | Sends full‑text `didChange` |
| `SaveFile` | Sends `didSave` |
| `GotoDefinition` | Sends `textDocument/definition`, stores `oneshot::Sender` in `pending` |
| `RequestFormatting` | Performs `didOpen`/`didChange`/`didSave` sequence, then sends `textDocument/formatting`. Delays 150ms to let server process save. |
| `RequestInlayHintsRange` | Deduplicates requests by URI+version, sends `textDocument/inlayHint` |
| `RequestSignatureHelp` | Sends `textDocument/signatureHelp` |
| `RequestCompletion` | Sends `textDocument/completion` with context |
| `ResolveCompletionItem` | If `supports_completion_resolve`, sends `completionItem/resolve` |
| `Shutdown` | Shuts down active LSP |
| `Error` | Forwards to editor as `AppMessage::LspError` |

### 5.5 Response Handling

`handle_server_msg()` processes incoming JSON messages:

- If the message has an `id` field and that ID exists in `pending`, it calls `resolve_pending()` with the appropriate `PendingKind`.
- If the message has a `method` equal to `textDocument/publishDiagnostics`, it parses the parameters and forwards `AppMessage::LspDiagnostics` to the editor.

`resolve_pending()` deserialises the `result` field into the expected type (e.g., `Vec<Location>` for goto definition, `Vec<InlayHint>` for inlay hints, etc.) and sends the result back via `AppSender`.

### 5.6 Lazy LSP Startup

If `OpenFile` is received and no LSP is active, `start_lsp_for_file()` inspects the file extension to determine the language and its configured LSP command (e.g., `rust-analyzer` for Rust, `typescript-language-server` for TS). It spawns the server, performs initialization, and immediately opens the current file. Only one LSP server is kept alive at a time – switching to a file of a different language will replace the server.

## 6. Integration with Editor Features

### 6.1 Diagnostics

The server sends `textDocument/publishDiagnostics` notifications. `LspManager` converts the LSP‑formatted diagnostics into the editor’s internal `Diagnostic` type (which stores line, column range, severity, message) and sends them to the main editor loop. The editor then draws squiggles and shows error messages.

### 6.2 Autocompletion

The editor’s completion subsystem (`editor/completion.rs`) uses `LspManager` to request completions. The flow:

1. User types a trigger character (e.g., `.` or `:`), or the editor detects that a word prefix has reached a minimum length.
2. `update_unified_completions()` calls `extract_cursor_context()` to get the prefix and trigger char.
3. Local completions (buffer words, vocabulary) are collected.
4. If an LSP server is active, a `LspMessage::RequestCompletion` is sent.
5. The LSP server responds with a `CompletionList` or array of `CompletionItem`s.
6. The editor merges LSP items with local items, filters them based on the prefix, and displays a popup.
7. On selection, `apply_completion()` handles the insertion: it respects `textEdit` or `insertText`, applies additional `TextEdit`s, and triggers snippet mode if the completion is a snippet.

### 6.3 Ghost Text

When the completion popup is active, the editor computes a “ghost text” – the remaining suffix of the selected completion that would be inserted. It is displayed faintly after the cursor. Pressing `<Tab>` accepts the ghost text without closing the popup.

### 6.4 Snippets

If a completion item has `insert_text_format = 2` (snippet), the insertion process uses `insert_snippet_at()`, which parses the snippet string (e.g., `fn ${1:name}(${2:params}) { ${3:body} }`) and returns tab stop positions. The editor then enters snippet mode, highlighting the first tab stop. Pressing `<Tab>` jumps to the next stop, and `<Esc>` exits snippet mode, placing the cursor at the final position.

### 6.5 Formatting

The editor can request formatting of the whole document. `LspMessage::RequestFormatting` triggers a sequence:

- If the file is not already open, send `didOpen` with the current text.
- Otherwise, send a `didChange` with the latest text (version incremented).
- Send `didSave` (with optional text).
- Wait 150 ms for the server to process the save.
- Send `textDocument/formatting` with formatting options (tab size, insert spaces, etc.).
- The server returns a `Vec<TextEdit>`.
- The editor applies the edits, updating the buffer and cursor position accordingly.

### 6.6 Goto Definition

The editor sends a `LspMessage::GotoDefinition` containing a `oneshot::Sender`. The LSP server’s response (one or more `Location`s) is sent back through that channel. The editor then moves the cursor to the target location or opens the file if needed.

### 6.7 Inlay Hints

Inlay hints (e.g., type annotations, parameter names) are requested asynchronously for the visible range of the current buffer. `LspManager` deduplicates requests by URI and version, so only the most recent version’s request is kept. The response (list of `InlayHint`) is sent to the editor, which renders them as greyed‑out virtual text.

### 6.8 Signature Help

When the user types `(` or `<Tab>` inside a function call, the editor requests signature help. The LSP server returns a `SignatureHelp` structure containing function overloads, parameters, and the active parameter. The editor displays a popup showing the current signature and highlights the active parameter. It also provides multi‑line display when multiple overloads exist.

## 7. Error Handling and Resilience

- **Server crashes**: If the background reader task exits or the `rx` channel closes, `LspManager` clears all pending requests (sending empty results or error messages back to the editor) and drops `active_lsp`. Subsequent file opens will attempt to restart the server.
- **Timeouts**: Only the blocking `initialize` request has a timeout (15 seconds). All other requests are non‑blocking and rely on the server’s eventual response; if the server never responds, the pending entry remains forever. To prevent leaks, the editor may eventually time out at the UI level (e.g., after 2 seconds for completions).
- **Invalid JSON**: The reader task silently ignores malformed messages; it continues reading the next frame. This prevents a single corrupt message from crashing the whole subsystem.
- **File version tracking**: When a `didChange` is sent, the version number increments. If an inlay hint request arrives with an older version than the current file version, it is discarded to avoid stale hints.
- **Diagnostic clearing**: Before sending a `didChange`, `LspManager` optimistically clears diagnostics for that file to remove obsolete squiggles until the server publishes new ones.

## 8. Configuration and Capabilities

Capabilities are negotiated during `initialize`:

- The client advertises support for:
  - `textDocument/synchronization` (willSave, didSave)
  - `textDocument/signatureHelp` (context support, parameter label offsets)
  - `textDocument/completion` (snippet support, resolve support for documentation/detail/additionalTextEdits)
  - Position encodings: `["utf-8", "utf-16", "utf-32"]`
- The server’s response determines the effective `offset_encoding`, `supports_snippets`, and `supports_completion_resolve`.
- The `initialization_options` currently hard‑code rust‑analyzer specific settings (inlay hints configuration). This should be made configurable per language in the future.

## 9. Concurrency Model

- The editor’s main thread runs the UI and event handling synchronously.
- `LspManager` runs in a separate tokio task. All communication with the editor goes through `AppMessage` (via a cross‑thread channel) and `LspMessage` (an unbounded channel).
- The background reader task runs as a third tokio task, purely forwarding bytes to JSON messages.
- No `Mutex` or `RwLock` is needed because `LspManager` has exclusive ownership of the `LanguageServer` and its state within its own task.

## 10. Usage Example (from editor integration)

```rust
// Get a sender to LspManager
let lsp_tx = lsp_manager.get_sender();

// Open a file
lsp_tx.send(LspMessage::OpenFile(path.clone()))?;

// Change content incrementally
lsp_tx.send(LspMessage::ChangeFileIncremental {
    path: path.clone(),
    version: 5,
    start_line: 10, start_char: 4,
    end_line: 10, end_char: 8,
    new_text: "x".to_string(),
})?;

// Request completions
lsp_tx.send(LspMessage::RequestCompletion(
    path, line, col, Some(".".to_string())
))?;
```

Responses are received via `AppMessage` handlers in the editor loop.

## 11. Limitations & Future Work

- Only one LSP server can be active at a time. Switching language restarts the server, losing all state.
- No support for dynamic registration of capabilities.
- No support for `workspace/didChangeConfiguration`.
- Hardcoded rust‑analyzer `initialization_options` – should be language‑specific and user‑configurable.
- No re‑request of inlay hints after scrolling – currently only requested for a fixed range around the cursor; full viewport hints would be better.
- Timeouts for pending requests are not implemented at the LSP manager level – could cause indefinite blocking if the server never responds.
- No logging framework – uses `#[cfg(feature = "debug_log")]` ad‑hoc logging.

---

This specification is derived directly from the implementation in `lsp.rs`, `completion.rs`, `snippet.rs`, and `text_object.rs`. It serves as a reference for maintainers and as a basis for future enhancements.