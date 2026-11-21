//! Keyboard actions and their default bindings.

use gpui::{App, KeyBinding, actions};

/// Key context of the file list.
pub(crate) const FILE_LIST: &str = "FileList";

/// Key context of the name field while a row is renamed.
pub(crate) const FILE_RENAME: &str = "FileRename";

/// Key context of a pane (file list or server panel).
pub(crate) const PANE: &str = "Pane";

/// Key context of the "go to folder" field in a pane's header.
pub(crate) const GO_TO: &str = "GoToFolder";

actions!(
    pane,
    [
        /// Closes the pane's connection and shows the servers.
        Disconnect,
        /// Turns the path bar into a field for a path or an address.
        GoToFolder,
        CancelGoTo,
        /// Switches the pane between its files and the server panel.
        ToggleServers,
        FocusLeftPane,
        FocusRightPane,
    ]
);

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
        NewFolder,
        NewFile,
        /// Edits the selected entry's name in place.
        Rename,
        CommitRename,
        CancelRename,
        /// Deletes the selection, after asking.
        Delete,
        /// Copies the selection next to itself ("name (2)").
        Duplicate,
        /// Puts the addresses of the selection on the clipboard.
        CopyAddress,
        ToggleHiddenFiles,
    ]
);

/// Registers the default key bindings.
pub(crate) fn bind_keys(cx: &mut App) {
    let pane = Some(PANE);
    cx.bind_keys([KeyBinding::new("escape", CancelGoTo, Some(GO_TO))]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-e", Disconnect, pane),
        KeyBinding::new("cmd-l", GoToFolder, pane),
        KeyBinding::new("cmd-shift-g", GoToFolder, pane),
        KeyBinding::new("cmd-shift-k", ToggleServers, pane),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-e", Disconnect, pane),
        KeyBinding::new("ctrl-l", GoToFolder, pane),
        KeyBinding::new("ctrl-shift-g", GoToFolder, pane),
        KeyBinding::new("ctrl-shift-k", ToggleServers, pane),
    ]);
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
        KeyBinding::new("f2", Rename, context),
        KeyBinding::new("escape", CancelRename, Some(FILE_RENAME)),
        KeyBinding::new("tab", CommitRename, Some(FILE_RENAME)),
        // Enter confirms the name; it must not also open the folder.
        KeyBinding::new("enter", CommitRename, Some(FILE_RENAME)),
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
        KeyBinding::new("cmd-shift-n", NewFolder, context),
        KeyBinding::new("cmd-alt-n", NewFile, context),
        KeyBinding::new("cmd-backspace", Delete, context),
        KeyBinding::new("cmd-d", Duplicate, context),
        KeyBinding::new("cmd-shift-c", CopyAddress, context),
        KeyBinding::new("cmd-shift-r", CopyToOtherPane, context),
        KeyBinding::new("cmd-shift-.", ToggleHiddenFiles, context),
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
        KeyBinding::new("ctrl-shift-n", NewFolder, context),
        KeyBinding::new("ctrl-alt-n", NewFile, context),
        KeyBinding::new("delete", Delete, context),
        KeyBinding::new("ctrl-d", Duplicate, context),
        KeyBinding::new("ctrl-shift-c", CopyAddress, context),
        KeyBinding::new("ctrl-shift-r", CopyToOtherPane, context),
        KeyBinding::new("ctrl-h", ToggleHiddenFiles, context),
    ]);
}
