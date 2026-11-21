//! The sidebar: favorites in folders, hosts from the SSH configuration, and
//! this computer.

use ferry_core::Destination;
use ferry_core::library::{BookmarkDraft, BookmarkId, FolderId};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_component_assets::IconName as Lucide;

use super::Workspace;
use super::dialogs::{ConfirmAction, FolderPurpose};
use crate::session::ConnectRequest;

pub(super) const SIDEBAR_WIDTH: gpui::Pixels = px(240.);

/// Indentation per folder level.
const INDENT: f32 = 14.;

/// Folder nesting rendered at most; deeper folders still work, they just stop
/// indenting further.
const MAX_DEPTH: usize = 8;

/// Colors used by sidebar rows.
#[derive(Clone, Copy)]
struct Palette {
    muted: Hsla,
    hover: Hsla,
    active: Hsla,
    folder: Hsla,
}

impl Workspace {
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let palette = Palette {
            muted: theme.muted_foreground,
            hover: theme.sidebar_accent,
            active: theme.list_active,
            folder: theme.blue,
        };
        let background = theme.sidebar;
        let foreground = theme.sidebar_foreground;
        let active_bookmark = self
            .active_session()
            .and_then(|session| session.read(cx).bookmark());

        let mut favorites = Vec::new();
        self.render_folder_contents(None, 0, active_bookmark, palette, cx, &mut favorites);
        let empty = self.snapshot.bookmarks.is_empty() && self.snapshot.folders.is_empty();

        let hosts = self.render_ssh_hosts(palette, cx);

        let local = row(
            "local-files",
            Icon::new(Lucide::HardDrive).text_color(palette.muted),
            "Local files".into(),
            false,
            0,
            palette,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.open(
                ConnectRequest::to(Destination::Local { path: None }),
                false,
                window,
                cx,
            );
        }));

        v_flex()
            .id("sidebar")
            .w(SIDEBAR_WIDTH)
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .py_2()
            .gap_4()
            .bg(background)
            .text_color(foreground)
            .text_sm()
            .child(
                v_flex()
                    .child(Self::render_section_header(cx))
                    .children(favorites)
                    .when(empty, |this| {
                        let hint = if cfg!(target_os = "macos") {
                            "Save servers you use often: connect and press ⌘D, or use + above."
                        } else {
                            "Save servers you use often: connect and press Ctrl+D, or use + above."
                        };
                        this.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(hint),
                        )
                    }),
            )
            .when(!self.ssh_hosts.is_empty(), |this| {
                this.child(
                    v_flex()
                        .child(section_title("SSH CONFIG", palette.muted))
                        .children(hosts),
                )
            })
            .child(
                v_flex()
                    .child(section_title("THIS COMPUTER", palette.muted))
                    .child(local),
            )
    }

    /// One row per host alias from the SSH configuration.
    fn render_ssh_hosts(&self, palette: Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.ssh_hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let address = host.clone();
                let request = Destination::parse(host).ok().map(ConnectRequest::to);
                row(
                    ElementId::NamedInteger("ssh-host".into(), index as u64),
                    Icon::new(Lucide::SquareTerminal).text_color(palette.muted),
                    host.clone(),
                    false,
                    0,
                    palette,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    if let Some(request) = request.clone() {
                        this.open(request, false, window, cx);
                    }
                }))
                .context_menu({
                    let this = cx.entity().downgrade();
                    move |menu, _, _| {
                        let open_in_tab = {
                            let this = this.clone();
                            let address = address.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                if let Ok(destination) = Destination::parse(&address) {
                                    let _ = this.update(cx, |workspace, cx| {
                                        workspace.open(
                                            ConnectRequest::to(destination),
                                            true,
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }
                        };
                        let save = {
                            let this = this.clone();
                            let address = address.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                if let Ok(destination) = Destination::parse(&address) {
                                    let draft = BookmarkDraft {
                                        folder: None,
                                        name: address.to_string(),
                                        destination,
                                        identity_file: None,
                                    };
                                    let _ = this.update(cx, |workspace, cx| {
                                        workspace.open_editor(None, &draft, window, cx);
                                    });
                                }
                            }
                        };
                        menu.item(PopupMenuItem::new("Open in new tab").on_click(open_in_tab))
                            .item(PopupMenuItem::new("Save as favorite…").on_click(save))
                    }
                })
                .into_any_element()
            })
            .collect()
    }

    fn render_section_header(cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let this = cx.entity().downgrade();
        h_flex()
            .pl_3()
            .pr_1()
            .h(px(26.))
            .justify_between()
            .child(section_label("FAVORITES", muted))
            .child(
                Button::new("favorites-add")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Lucide::Plus))
                    .dropdown_menu(move |menu, _, _| {
                        let new_favorite = {
                            let this = this.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let _ = this.update(cx, |workspace, cx| {
                                    workspace.new_favorite(None, window, cx);
                                });
                            }
                        };
                        let new_folder = {
                            let this = this.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let _ = this.update(cx, |workspace, cx| {
                                    workspace.ask_folder_name(
                                        FolderPurpose::Create { parent: None },
                                        window,
                                        cx,
                                    );
                                });
                            }
                        };
                        let clear_history = {
                            let this = this.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let _ = this.update(cx, |workspace, cx| {
                                    workspace.confirm(ConfirmAction::ClearHistory, window, cx);
                                });
                            }
                        };
                        menu.item(PopupMenuItem::new("New favorite…").on_click(new_favorite))
                            .item(PopupMenuItem::new("New folder…").on_click(new_folder))
                            .separator()
                            .item(
                                PopupMenuItem::new("Clear recent connections…")
                                    .on_click(clear_history),
                            )
                    }),
            )
    }

    /// Appends the rows of `folder`'s subfolders and bookmarks to `rows`.
    fn render_folder_contents(
        &self,
        folder: Option<FolderId>,
        depth: usize,
        active_bookmark: Option<BookmarkId>,
        palette: Palette,
        cx: &mut Context<Self>,
        rows: &mut Vec<AnyElement>,
    ) {
        let subfolders: Vec<_> = self
            .snapshot
            .subfolders(folder)
            .map(|folder| (folder.id, folder.name.clone()))
            .collect();
        for (id, name) in subfolders {
            let open = !self.collapsed.contains(&id);
            rows.push(Self::render_folder_row(id, name, open, depth, palette, cx));
            if open {
                self.render_folder_contents(
                    Some(id),
                    (depth + 1).min(MAX_DEPTH),
                    active_bookmark,
                    palette,
                    cx,
                    rows,
                );
            }
        }
        let bookmarks: Vec<_> = self
            .snapshot
            .bookmarks_in(folder)
            .map(|bookmark| {
                let icon = match bookmark.destination {
                    Destination::Local { .. } => Lucide::HardDrive,
                    Destination::Sftp(_) => Lucide::Globe,
                };
                (bookmark.id, bookmark.name.clone(), icon)
            })
            .collect();
        for (id, name, icon) in bookmarks {
            let active = active_bookmark == Some(id);
            rows.push(Self::render_bookmark_row(
                id, name, icon, active, depth, palette, cx,
            ));
        }
    }

    fn render_folder_row(
        id: FolderId,
        name: String,
        open: bool,
        depth: usize,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let chevron = Icon::new(if open {
            Lucide::ChevronDown
        } else {
            Lucide::ChevronRight
        })
        .size_3()
        .text_color(palette.muted);
        row(
            ElementId::Name(format!("folder-{id:?}").into()),
            h_flex().gap_1().child(chevron).child(
                Icon::new(if open {
                    Lucide::FolderOpen
                } else {
                    Lucide::Folder
                })
                .text_color(palette.folder),
            ),
            name.into(),
            false,
            depth,
            palette,
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            if !this.collapsed.remove(&id) {
                this.collapsed.insert(id);
            }
            cx.notify();
        }))
        .context_menu(move |menu, _, _| {
            let action =
                |run: fn(&mut Workspace, FolderId, &mut Window, &mut Context<Workspace>)| {
                    let this = this.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |workspace, cx| run(workspace, id, window, cx));
                    }
                };
            menu.item(PopupMenuItem::new("New favorite here…").on_click(action(
                |workspace, id, window, cx| workspace.new_favorite(Some(id), window, cx),
            )))
            .item(PopupMenuItem::new("New folder inside…").on_click(action(
                |workspace, id, window, cx| {
                    workspace.ask_folder_name(
                        FolderPurpose::Create { parent: Some(id) },
                        window,
                        cx,
                    );
                },
            )))
            .separator()
            .item(
                PopupMenuItem::new("Rename…").on_click(action(|workspace, id, window, cx| {
                    workspace.ask_folder_name(FolderPurpose::Rename(id), window, cx);
                })),
            )
            .item(
                PopupMenuItem::new("Delete…").on_click(action(|workspace, id, window, cx| {
                    workspace.confirm(ConfirmAction::DeleteFolder(id), window, cx);
                })),
            )
        })
        .into_any_element()
    }

    fn render_bookmark_row(
        id: BookmarkId,
        name: String,
        icon: Lucide,
        active: bool,
        depth: usize,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        row(
            ElementId::Name(format!("bookmark-{id:?}").into()),
            Icon::new(icon).text_color(palette.muted),
            name.into(),
            active,
            depth,
            palette,
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            let new_tab = event.modifiers().secondary(cx);
            this.open_bookmark(id, new_tab, window, cx);
        }))
        .context_menu(move |menu, _, _| {
            let action =
                |run: fn(&mut Workspace, BookmarkId, &mut Window, &mut Context<Workspace>)| {
                    let this = this.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |workspace, cx| run(workspace, id, window, cx));
                    }
                };
            menu.item(
                PopupMenuItem::new("Open").on_click(action(|workspace, id, window, cx| {
                    workspace.open_bookmark(id, false, window, cx);
                })),
            )
            .item(PopupMenuItem::new("Open in new tab").on_click(action(
                |workspace, id, window, cx| workspace.open_bookmark(id, true, window, cx),
            )))
            .separator()
            .item(
                PopupMenuItem::new("Edit…").on_click(action(|workspace, id, window, cx| {
                    workspace.edit_bookmark(id, window, cx);
                })),
            )
            .item(
                PopupMenuItem::new("Duplicate").on_click(action(|workspace, id, _, cx| {
                    workspace.duplicate_bookmark(id, cx);
                })),
            )
            .item(
                PopupMenuItem::new("Delete…").on_click(action(|workspace, id, window, cx| {
                    workspace.confirm(ConfirmAction::DeleteBookmark(id), window, cx);
                })),
            )
        })
        .into_any_element()
    }

    fn new_favorite(
        &mut self,
        folder: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Prefill with the current location, which is what one usually saves.
        let destination = self
            .active_session()
            .and_then(|session| session.read(cx).location(cx))
            .unwrap_or(Destination::Local { path: None });
        let draft = BookmarkDraft {
            folder,
            name: String::new(),
            destination,
            identity_file: None,
        };
        self.open_editor(None, &draft, window, cx);
    }

    fn edit_bookmark(&mut self, id: BookmarkId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bookmark) = self.snapshot.bookmark(id).cloned() {
            self.open_editor(Some(id), &bookmark.into(), window, cx);
        }
    }

    fn duplicate_bookmark(&mut self, id: BookmarkId, cx: &mut Context<Self>) {
        let Some(bookmark) = self.snapshot.bookmark(id).cloned() else {
            return;
        };
        let mut draft = BookmarkDraft::from(bookmark);
        draft.name = format!("{} copy", draft.name);
        self.change_library(
            move |library| library.create_bookmark(&draft).map(|_| ()),
            cx,
        );
    }
}

fn section_label(text: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(text)
}

fn section_title(text: &'static str, color: Hsla) -> impl IntoElement {
    h_flex().px_3().h(px(26.)).child(section_label(text, color))
}

/// A clickable sidebar row with an icon and a truncated label.
fn row(
    id: impl Into<ElementId>,
    icon: impl IntoElement,
    label: SharedString,
    active: bool,
    depth: usize,
    palette: Palette,
) -> gpui::Stateful<gpui::Div> {
    #[expect(clippy::cast_precision_loss, reason = "depth is at most MAX_DEPTH")]
    let indent = px(12. + INDENT * depth as f32);
    h_flex()
        .id(id)
        .mx_1()
        .h(px(28.))
        .pl(indent)
        .pr_2()
        .gap_2()
        .rounded_md()
        .cursor_pointer()
        .when(active, |this| this.bg(palette.active))
        .when(!active, |this| this.hover(|style| style.bg(palette.hover)))
        .child(icon)
        .child(div().flex_1().min_w_0().truncate().child(label))
}
