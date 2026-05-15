// src/command_macros.rs
//!
//! Declarative macro for bulk-registering editor commands into the
//! `CommandRegistry`.  This is the **single source of truth** for all
//! `:` command names, aliases, descriptions, and handlers.
//!
//! ## Syntax
//!
//! ```ignore
//! register_commands! { registry;
//!     // name, alias1, alias2, … => description => |editor, args| { body };
//!     "q", "quit", "qa", "qall" => "Quit (fails if dirty)" => |e, _| {
//!         if e.buffers.iter().any(|b| b.dirty) {
//!             CommandResult::Error("Unsaved changes!".into())
//!         } else {
//!             e.should_quit = true;
//!             CommandResult::Quit
//!         }
//!     };
//! }
//! ```
//!
//! Each entry:
//! - **name** (literal) — canonical command name
//! - **aliases** (zero or more literals, comma-separated) — alternative names
//! - **description** (literal) — shown in completion popups
//! - **handler** (closure) — `Fn(&mut Editor, &str) -> CommandResult`
//!
//! The closure receives:
//! - `&mut Editor` — full mutable access to editor state
//! - `&str` — the argument string after the command name (e.g. `"main.rs"` from `:e main.rs`)
//!
//! ## Notes
//!
//! - Aliases are resolved at dispatch time: typing `:quit` resolves to `"q"`.
//! - All closures must be `Send + 'static` (they are boxed in `CommandEntry`).
//! - Inside `build_command_registry()`, `use CommandResult::*;` is in scope,
//!   so you can write `Error("msg")` instead of `CommandResult::Error("msg")`.

/// Bulk-register commands into a [`CommandRegistry`](crate::command::CommandRegistry).
///
/// See module-level documentation for syntax and examples.
macro_rules! register_commands {
    (
        $registry:expr_2021;
        $(
            $name:literal $(, $alias:literal)* => $desc:literal => $handler:expr_2021
        );* $(;)?
    ) => {
        $(
            $registry.register_handler($name, $handler, $desc);
            $(
                $registry.alias($alias, $name);
            )*
        )*
    };
}
