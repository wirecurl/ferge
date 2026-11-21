//! Keyboard actions and their default bindings.

use gpui::{App, KeyBinding, actions};

/// Key context of the file list.
pub(crate) const FILE_LIST: &str = "FileList";

actions!(
    file_list,
    [
        SelectPrevious,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        /// Enters the selected directory.
        Open,
        GoToParent,
        GoBack,
        GoForward,
        /// Deletes the last filter character, or goes to the parent directory
        /// when no filter is active.
        Backspace,
        ClearFilter,
        SortByName,
        SortBySize,
        SortByModified,
        Refresh,
        /// Extends the selection by one row up or down.
        SelectPreviousExtend,
        SelectNextExtend,
        SelectAll,
        /// Remembers the selected items for pasting elsewhere.
        Copy,
        /// Copies the remembered items into the current folder.
        Paste,
        /// Copies the selection into the folder of the other pane.
        CopyToOtherPane,
        /// Moves the keyboard focus to the other pane.
        FocusOtherPane,
    ]
);

/// Registers the default key bindings.
pub(crate) fn bind_keys(cx: &mut App) {
    let context = Some(FILE_LIST);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("pageup", SelectPageUp, context),
        KeyBinding::new("pagedown", SelectPageDown, context),
        KeyBinding::new("home", SelectFirst, context),
        KeyBinding::new("end", SelectLast, context),
        KeyBinding::new("enter", Open, context),
        KeyBinding::new("backspace", Backspace, context),
        KeyBinding::new("escape", ClearFilter, context),
        KeyBinding::new("shift-up", SelectPreviousExtend, context),
        KeyBinding::new("shift-down", SelectNextExtend, context),
        KeyBinding::new("f5", CopyToOtherPane, context),
        KeyBinding::new("tab", FocusOtherPane, context),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-down", Open, context),
        KeyBinding::new("cmd-up", GoToParent, context),
        KeyBinding::new("cmd-[", GoBack, context),
        KeyBinding::new("cmd-]", GoForward, context),
        KeyBinding::new("cmd-1", SortByName, context),
        KeyBinding::new("cmd-2", SortBySize, context),
        KeyBinding::new("cmd-3", SortByModified, context),
        KeyBinding::new("cmd-r", Refresh, context),
        KeyBinding::new("cmd-a", SelectAll, context),
        KeyBinding::new("cmd-c", Copy, context),
        KeyBinding::new("cmd-v", Paste, context),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("alt-down", Open, context),
        KeyBinding::new("alt-up", GoToParent, context),
        KeyBinding::new("alt-left", GoBack, context),
        KeyBinding::new("alt-right", GoForward, context),
        KeyBinding::new("ctrl-1", SortByName, context),
        KeyBinding::new("ctrl-2", SortBySize, context),
        KeyBinding::new("ctrl-3", SortByModified, context),
        KeyBinding::new("ctrl-r", Refresh, context),
        KeyBinding::new("ctrl-a", SelectAll, context),
        KeyBinding::new("ctrl-c", Copy, context),
        KeyBinding::new("ctrl-v", Paste, context),
    ]);
}
