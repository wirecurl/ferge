//! The inspector: details of the selection in the active pane, with its
//! permissions to edit.

use std::sync::Arc;

use ferry_core::{Entry, EntryKind, RemotePath};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, WeakEntity, Window, div, rems,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::browser::FileBrowser;
use crate::file_type::FileType;
use crate::style::{self, TypeScale as _};
use crate::{format, runtime};

/// Width of the inspector.
pub(crate) const INSPECTOR_WIDTH: gpui::Rems = rems(21.);

/// Who a permission bit is for, in display order.
const CLASSES: [(&str, u32); 3] = [("Owner", 6), ("Group", 3), ("Everyone", 0)];

/// The three permission kinds, as bit offsets within a class.
const KINDS: [(&str, u32); 3] = [("Read", 2), ("Write", 1), ("Execute", 0)];

pub(crate) struct Inspector {
    browser: Option<WeakEntity<FileBrowser>>,
    observing: Option<Subscription>,
    /// Permission bits being edited, and the file they belong to.
    editing: Option<(RemotePath, u32)>,
    /// A message about the last change, e.g. an error.
    status: Option<SharedString>,
}

impl Inspector {
    pub(crate) fn new() -> Self {
        Self {
            browser: None,
            observing: None,
            editing: None,
            status: None,
        }
    }

    /// Follows the selection of `browser`.
    pub(crate) fn set_browser(
        &mut self,
        browser: Option<&Entity<FileBrowser>>,
        cx: &mut Context<Self>,
    ) {
        let same = match (&self.browser, browser) {
            (Some(current), Some(new)) => current.entity_id() == new.entity_id(),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.browser = browser.map(Entity::downgrade);
        self.observing = browser.map(|browser| cx.observe(browser, |_, _, cx| cx.notify()));
        self.editing = None;
        self.status = None;
        cx.notify();
    }

    fn apply_permissions(&mut self, cx: &mut Context<Self>) {
        let Some((path, mode)) = self.editing.clone() else {
            return;
        };
        let Some(browser) = self.browser.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let remote = Arc::clone(browser.read(cx).remote());
        let changing = runtime::spawn(cx, async move { remote.chmod(&path, mode).await });
        self.status = Some("Saving…".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = changing.await;
            let _ = this.update(cx, |inspector, cx| {
                match result {
                    Ok(Ok(())) => {
                        inspector.editing = None;
                        inspector.status = Some("Permissions changed.".into());
                        browser.update(cx, FileBrowser::reload);
                    }
                    Ok(Err(error)) => inspector.status = Some(error.to_string().into()),
                    Err(error) => inspector.status = Some(error.to_string().into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_single(
        &self,
        entry: &Entry,
        browser: &Entity<FileBrowser>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let path = browser.read(cx).path().join(&entry.name);
        let address = browser.read(cx).address_of(&path);
        let file_type = FileType::of(entry.name.as_str(), entry.kind());
        let (icon, color) = (file_type.icon(), file_type.color(cx.theme()));
        let fact = |index: usize, label: &'static str, value: SharedString| {
            let row = h_flex()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .w(rems(5.))
                        .flex_shrink_0()
                        .text_small()
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().text_small().child(value));
            style::divided(row, index, cx)
        };
        let size: SharedString = if entry.kind() == EntryKind::File {
            format!(
                "{} ({} bytes)",
                format::size(entry.metadata.size),
                format::count(usize::try_from(entry.metadata.size).unwrap_or(usize::MAX))
            )
            .into()
        } else {
            "—".into()
        };
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .child(Icon::new(icon).size_10().text_color(color))
                    .child(
                        div()
                            .text_center()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from(entry.name.as_str().to_owned())),
                    ),
            )
            .child(
                style::group(cx)
                    .child(fact(0, "Kind", file_type.label().into()))
                    .child(fact(1, "Size", size))
                    .child(fact(
                        2,
                        "Modified",
                        format::date(entry.metadata.modified).into(),
                    ))
                    .child(fact(3, "Where", address.clone().into())),
            )
            .child(
                Button::new("copy-address")
                    .small()
                    .label("Copy address")
                    .on_click(move |_: &ClickEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(address.clone()));
                    }),
            )
            // A link's own bits mean nothing, and changing them would change
            // the target's instead.
            .children(
                entry
                    .metadata
                    .permissions
                    .filter(|_| entry.kind() != EntryKind::Symlink)
                    .map(|mode| self.render_permissions(&path, mode, cx)),
            )
    }

    fn render_permissions(
        &self,
        path: &RemotePath,
        current: u32,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mode = match &self.editing {
            Some((editing, mode)) if editing == path => *mode,
            _ => current,
        };
        let changed = mode != current;
        let header = h_flex()
            .gap_1()
            .child(div().w(rems(5.)))
            .children(KINDS.iter().map(|(label, _)| {
                div()
                    .w(rems(4.))
                    .text_small()
                    .text_color(muted)
                    .child(*label)
            }));
        let rows = CLASSES.iter().map(|(class, class_shift)| {
            h_flex()
                .gap_1()
                .child(div().w(rems(5.)).text_small().child(*class))
                .children(KINDS.iter().map(|(kind, kind_shift)| {
                    let bit = 1 << (class_shift + kind_shift);
                    let path = path.clone();
                    div().w(rems(4.)).child(
                        Checkbox::new(SharedString::from(format!("perm-{class}-{kind}")))
                            .checked(mode & bit != 0)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let base = match &this.editing {
                                    Some((editing, mode)) if *editing == path => *mode,
                                    _ => current,
                                };
                                let mode = if *checked { base | bit } else { base & !bit };
                                this.editing = Some((path.clone(), mode));
                                this.status = None;
                                cx.notify();
                            })),
                    )
                }))
        });
        style::group(cx)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Permissions"),
                    )
                    .child(
                        div()
                            .text_small()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(format!("{mode:04o} {}", format::permissions(mode))),
                    ),
            )
            .child(header)
            .children(rows)
            .when(changed, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(Button::new("perm-revert").small().label("Revert").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.editing = None;
                                cx.notify();
                            }),
                        ))
                        .child(
                            Button::new("perm-apply")
                                .small()
                                .primary()
                                .label("Apply")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.apply_permissions(cx);
                                })),
                        ),
                )
            })
    }
}

impl Render for Inspector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, background) = (theme.muted_foreground, theme.background);
        let browser = self.browser.as_ref().and_then(WeakEntity::upgrade);
        let entries = browser
            .as_ref()
            .map(|browser| browser.read(cx).selected_entries())
            .unwrap_or_default();
        let body = match (&browser, entries.as_slice()) {
            (Some(browser), [entry]) => self.render_single(entry, browser, cx),
            (Some(_), entries) if entries.len() > 1 => {
                let files = entries
                    .iter()
                    .filter(|entry| entry.kind() == EntryKind::File);
                let (count, bytes) = files.fold((0_usize, 0_u64), |(count, bytes), entry| {
                    (count + 1, bytes + entry.metadata.size)
                });
                v_flex()
                    .items_center()
                    .gap_2()
                    .py_4()
                    .child(Icon::new(IconName::Copy).size_10().text_color(muted))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(format!("{} items", format::count(entries.len()))),
                    )
                    .child(div().text_body().text_color(muted).child(format!(
                        "{} in {} files",
                        format::size(bytes),
                        format::count(count)
                    )))
            }
            _ => v_flex()
                .py_4()
                .text_body()
                .text_color(muted)
                .child("Select a file or folder to see its details."),
        };
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(background)
            .child(
                div()
                    .text_body()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Info"),
            )
            .child(body)
            .children(
                self.status
                    .clone()
                    .map(|status| div().text_body().text_color(muted).child(status)),
            )
    }
}
