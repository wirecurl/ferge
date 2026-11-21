//! The window toolbar and the tab bar.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    ClickEvent, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
    px,
};
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
};

use super::{NewTab, ToggleInspector, ToggleSplit, ToggleTransfers, Workspace};
use crate::actions;

/// Height of the toolbar.
const TOOLBAR_HEIGHT: gpui::Pixels = px(44.);

impl Workspace {
    pub(super) fn render_toolbar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (border, background) = (theme.border, theme.title_bar);
        let connected = self.active_browser(cx).is_some();
        let tool = |id: &'static str, icon: IconName, tooltip: &'static str| {
            Button::new(id).ghost().small().icon(icon).tooltip(tooltip)
        };
        let split = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.panes.len() == 2);
        let navigation = self.render_navigation(cx);
        let dispatch = |action: Box<dyn gpui::Action>| {
            move |_: &ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                window.dispatch_action(action.boxed_clone(), cx);
            }
        };
        h_flex()
            .h(TOOLBAR_HEIGHT)
            .px_2()
            .gap_1()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .bg(background)
            .child(navigation)
            .child(separator(cx))
            .child(
                tool("toggle-split", IconName::PanelRight, "Two file browsers")
                    .selected(split)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.on_toggle_split(&ToggleSplit, window, cx);
                    })),
            )
            .children(self.render_narrow_switch(window, cx))
            .child(separator(cx))
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
            )
            .child(
                tool("info", IconName::Info, "Info")
                    .selected(self.inspector_open)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.on_toggle_inspector(&ToggleInspector, window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                div().w(px(240.)).child(
                    Input::new(&self.filter)
                        .small()
                        .prefix(Icon::new(IconName::Search).size_3p5())
                        .cleanable(true),
                ),
            )
            .child(self.render_transfers_toggle(cx))
            .child(
                tool("new-tab", IconName::Plus, "New tab").on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.on_new_tab(&NewTab, window, cx);
                    },
                )),
            )
    }

    /// Back and forward in the active pane.
    fn render_navigation(&self, cx: &mut Context<Self>) -> ButtonGroup {
        let (can_go_back, can_go_forward) =
            self.active_browser(cx).map_or((false, false), |browser| {
                let browser = browser.read(cx);
                (browser.can_go_back(), browser.can_go_forward())
            });
        ButtonGroup::new("navigation")
            .ghost()
            .small()
            .child(
                Button::new("back")
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back")
                    .disabled(!can_go_back),
            )
            .child(
                Button::new("forward")
                    .icon(IconName::ArrowRight)
                    .tooltip("Forward")
                    .disabled(!can_go_forward),
            )
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                if let Some(browser) = this.active_browser(cx) {
                    browser.update(cx, |browser, cx| {
                        if clicked.contains(&0) {
                            browser.go_back(cx);
                        } else {
                            browser.go_forward(cx);
                        }
                    });
                }
            }))
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
            ButtonGroup::new("pane-switch")
                .small()
                .outline()
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
        Button::new("toggle-transfers")
            .ghost()
            .small()
            .icon(IconName::ArrowDown)
            .when_some(label, Button::label)
            .tooltip("Transfers")
            .selected(self.transfers_open)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.on_toggle_transfers(&ToggleTransfers, window, cx);
            }))
    }

    /// Tabs, shown once there is more than one.
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.tabs.len() < 2 {
            return None;
        }
        let this = cx.entity().downgrade();
        let tabs: Vec<Tab> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let session = tab.active().read(cx);
                let close = {
                    let this = this.clone();
                    Button::new(("close-tab", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(move |_: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            let _ = this.update(cx, |workspace, cx| {
                                workspace.close_tab(index, window, cx);
                            });
                        })
                };
                let icon = if session.is_connecting() {
                    IconName::LoaderCircle
                } else {
                    IconName::Globe
                };
                Tab::new()
                    .prefix(div().pl_2().child(Icon::new(icon).size_3p5()))
                    .label(session.title(cx))
                    .suffix(close)
            })
            .collect();
        Some(
            TabBar::new("tabs")
                .small()
                .selected_index(self.active)
                .on_click(move |index: &usize, window, cx| {
                    let index = *index;
                    let _ = this.update(cx, |workspace, cx| workspace.activate(index, window, cx));
                })
                .children(tabs),
        )
    }
}

fn separator(cx: &Context<Workspace>) -> impl IntoElement {
    div().w(px(1.)).h(px(20.)).mx_1().bg(cx.theme().border)
}
