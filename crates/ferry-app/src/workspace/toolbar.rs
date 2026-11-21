//! The window toolbar: navigation, the tabs, the search field and the tools.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Icon, IconName, Selectable as _, h_flex};

use super::{NewTab, ToggleInspector, ToggleSplit, ToggleTransfers, Workspace};
use crate::actions;
use crate::settings_window::OpenSettings;
use crate::style::{self, ControlSize, IconButton, Palette as _, TypeScale as _, em};

impl Workspace {
    pub(super) fn render_toolbar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (border, background) = (theme.border, theme.background);
        let connected = self.active_browser(cx).is_some();
        let split = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.len() == 2);
        let dispatch = |action: Box<dyn gpui::Action>| {
            move |_: &ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                window.dispatch_action(action.boxed_clone(), cx);
            }
        };
        let file_tools = style::track(ControlSize::Regular, cx)
            .child(
                tool("new-folder", IconName::FolderClosed, "New folder")
                    .disabled(!connected)
                    .on_click(dispatch(Box::new(actions::NewFolder))),
            )
            .child(
                tool("transfer", IconName::ArrowRight, "Copy to the other side")
                    .disabled(!connected || !split)
                    .on_click(dispatch(Box::new(actions::CopyToOtherPane))),
            )
            .child(
                tool("delete", IconName::Delete, "Delete")
                    .disabled(!connected)
                    .on_click(dispatch(Box::new(actions::Delete))),
            );
        let view_tools = style::track(ControlSize::Regular, cx)
            .child(
                tool("toggle-split", IconName::PanelRight, "Two file browsers")
                    .selected(split)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.on_toggle_split(&ToggleSplit, window, cx);
                    })),
            )
            .child(
                tool("info", IconName::Info, "Info")
                    .selected(self.inspector_open)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.on_toggle_inspector(&ToggleInspector, window, cx);
                    })),
            )
            .child(
                tool("toggle-transfers", IconName::ArrowDown, "Transfers")
                    .selected(self.transfers_open)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.on_toggle_transfers(&ToggleTransfers, window, cx);
                    })),
            )
            .children(self.render_progress(cx))
            .child(
                tool("settings", IconName::Settings, "Settings")
                    .on_click(dispatch(Box::new(OpenSettings))),
            );
        let search = style::field(
            Input::new(&self.filter)
                .prefix(Icon::new(IconName::Search).size(ControlSize::Regular.icon()))
                .cleanable(true),
            ControlSize::Regular,
            cx,
        );
        h_flex()
            .h(style::TOOLBAR_HEIGHT)
            .px(style::BAR_PADDING)
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .bg(background)
            .child(self.render_navigation(cx))
            .child(self.render_tabs(cx))
            .children(self.render_narrow_switch(window, cx))
            .child(div().flex_1().min_w_2())
            .child(div().w(em(18.)).flex_shrink_1().min_w(em(8.)).child(search))
            .child(file_tools)
            .child(view_tools)
    }

    /// Back and forward in the active pane.
    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (can_go_back, can_go_forward) =
            self.active_browser(cx).map_or((false, false), |browser| {
                let browser = browser.read(cx);
                (browser.can_go_back(), browser.can_go_forward())
            });
        let go = |back: bool| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                if let Some(browser) = this.active_browser(cx) {
                    browser.update(cx, |browser, cx| {
                        if back {
                            browser.go_back(cx);
                        } else {
                            browser.go_forward(cx);
                        }
                    });
                }
            })
        };
        style::track(ControlSize::Regular, cx)
            .child(
                tool("back", IconName::ArrowLeft, "Back")
                    .disabled(!can_go_back)
                    .on_click(go(true)),
            )
            .child(
                tool("forward", IconName::ArrowRight, "Forward")
                    .disabled(!can_go_forward)
                    .on_click(go(false)),
            )
    }

    /// The tabs as a segmented control, with the button for a new one.
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (raised, muted, foreground, hover) = (
            theme.raised_fill(),
            theme.muted_foreground,
            theme.foreground,
            theme.hover_fill(),
        );
        let closable = self.tabs.len() > 1;
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let session = tab.active().read(cx);
                let is_local = session
                    .browser()
                    .is_some_and(|browser| browser.read(cx).remote().is_local());
                let icon = if session.is_connecting() {
                    IconName::LoaderCircle
                } else if is_local {
                    IconName::HardDrive
                } else {
                    IconName::Globe
                };
                let title = session.title(cx);
                let active = index == self.active;
                let close = closable.then(|| {
                    IconButton::new(("close-tab", index), IconName::Close)
                        .size(ControlSize::Compact)
                        .in_track()
                        .tooltip("Close tab")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(index, window, cx);
                        }))
                });
                h_flex()
                    .id(ElementId::NamedInteger("tab".into(), index as u64))
                    .h(ControlSize::Regular.item())
                    .pl(em(0.625))
                    // The close button is a square of its own; text alone
                    // gets the same space on both sides.
                    .pr(if closable { em(0.15) } else { em(0.625) })
                    .gap(em(0.5))
                    .min_w_0()
                    .rounded(style::RADIUS)
                    .cursor_pointer()
                    .text_body()
                    .map(|this| {
                        if active {
                            this.bg(raised).text_color(foreground).shadow_xs()
                        } else {
                            this.text_color(muted).hover(|style| style.bg(hover))
                        }
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.activate(index, window, cx);
                    }))
                    .child(Icon::new(icon).size(em(1.)).flex_shrink_0())
                    .child(div().max_w(em(12.)).truncate().child(title))
                    .children(close)
            })
            .collect();
        style::track(ControlSize::Regular, cx)
            .flex_shrink(1.)
            .min_w_0()
            .overflow_hidden()
            .children(tabs)
            .child(
                tool("new-tab", IconName::Plus, "New tab").on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.on_new_tab(&NewTab, window, cx);
                    },
                )),
            )
    }

    /// In a narrow window only one pane fits; this switches between them.
    fn render_narrow_switch(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let tab = self.tabs.get(self.active)?;
        if tab.panes.len() < 2 || !Self::is_narrow(window) {
            return None;
        }
        let choices: Vec<(usize, SharedString)> = tab
            .panes
            .iter()
            .enumerate()
            .map(|(index, pane)| (index, pane.read(cx).title(cx)))
            .collect();
        let this = cx.entity().downgrade();
        Some(style::segmented(
            "pane-switch",
            ControlSize::Regular,
            &choices,
            tab.focused,
            move |index, window, cx| {
                let _ = this.update(cx, |workspace, cx| workspace.focus_pane(index, window, cx));
            },
            cx,
        ))
    }

    /// Overall progress of running transfers, next to their button.
    fn render_progress(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let panel = self.transfer_panel.read(cx);
        let progress = panel
            .overall_progress()
            .filter(|_| panel.active_count() > 0)?;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a percentage between 0 and 100"
        )]
        let percent = (progress * 100.0).round() as u32;
        Some(
            div()
                .px(em(0.25))
                .text_small()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{percent}%")),
        )
    }
}

/// An icon button inside a toolbar track.
fn tool(id: &'static str, icon: IconName, tooltip: &'static str) -> IconButton {
    IconButton::new(id, icon).in_track().tooltip(tooltip)
}
