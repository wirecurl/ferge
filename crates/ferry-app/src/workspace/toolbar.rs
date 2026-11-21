//! The window toolbar: navigation, the tabs, the search field and the tools.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, rems,
};
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
};

use super::{NewTab, ToggleInspector, ToggleSplit, ToggleTransfers, Workspace};
use crate::actions;
use crate::settings_window::OpenSettings;
use crate::style::{self, Palette as _, TypeScale as _};

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
        let file_tools = style::track(cx)
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
        let view_tools = style::track(cx)
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
            .child(self.render_transfers_toggle(cx))
            .child(
                tool("settings", IconName::Settings, "Settings")
                    .on_click(dispatch(Box::new(OpenSettings))),
            );
        h_flex()
            .h(style::TOOLBAR_HEIGHT)
            .px_2()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .bg(background)
            .child(self.render_navigation(cx))
            .child(self.render_tabs(cx))
            .children(self.render_narrow_switch(window, cx))
            .child(div().flex_1().min_w_2())
            .child(
                div().w(rems(18.)).flex_shrink_1().min_w(rems(8.)).child(
                    Input::new(&self.filter)
                        .prefix(Icon::new(IconName::Search).size_4())
                        .cleanable(true),
                ),
            )
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
        style::track(cx)
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
                    Button::new(("close-tab", index))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close).size_3())
                        .tooltip("Close tab")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(index, window, cx);
                        }))
                });
                h_flex()
                    .id(ElementId::NamedInteger("tab".into(), index as u64))
                    .h(rems(2.))
                    .pl_2()
                    // The close button brings its own padding.
                    .map(|this| if closable { this.pr_0p5() } else { this.pr_2() })
                    .gap_1p5()
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
                    .child(Icon::new(icon).size_4().flex_shrink_0())
                    .child(div().max_w(rems(11.)).truncate().child(title))
                    .children(close)
            })
            .collect();
        style::track(cx)
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
        let titles: Vec<SharedString> = tab
            .panes
            .iter()
            .map(|pane| pane.read(cx).title(cx))
            .collect();
        let focused = tab.focused;
        Some(
            style::track(cx).child(
                ButtonGroup::new("pane-switch")
                    .ghost()
                    .children(titles.into_iter().enumerate().map(|(index, title)| {
                        Button::new(("pane", index))
                            .label(title)
                            .selected(index == focused)
                    }))
                    .on_click(cx.listener(|this, clicked: &Vec<usize>, window, cx| {
                        if let Some(&index) = clicked.first() {
                            this.focus_pane(index, window, cx);
                        }
                    })),
            ),
        )
    }

    /// The toolbar button that shows the transfers, with overall progress.
    fn render_transfers_toggle(&self, cx: &mut Context<Self>) -> Button {
        let panel = self.transfer_panel.read(cx);
        let label: Option<SharedString> = match panel.overall_progress() {
            Some(progress) if panel.active_count() > 0 => {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a percentage between 0 and 100"
                )]
                let percent = (progress * 100.0).round() as u32;
                Some(format!("{percent}%").into())
            }
            _ => None,
        };
        tool("toggle-transfers", IconName::ArrowDown, "Transfers")
            .when_some(label, Button::label)
            .selected(self.transfers_open)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.on_toggle_transfers(&ToggleTransfers, window, cx);
            }))
    }
}

/// An icon button inside a track.
fn tool(id: &'static str, icon: IconName, tooltip: &'static str) -> Button {
    Button::new(id)
        .ghost()
        .icon(Icon::new(icon).size_4())
        .tooltip(tooltip)
}
