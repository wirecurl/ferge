//! The window's transfer queue and the panel that shows it.
//!
//! The queue lives in a GPUI global so any view (a file list receiving a
//! drop, a paste) can start a transfer. Conflicts are asked through the
//! window's dialogs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ferry_core::transfer::{
    Conflict, ConflictDecision, ConflictResolver, DEFAULT_PARALLEL_FILES, Direction, TransferEvent,
    TransferId, TransferQueue, TransferRequest, TransferSnapshot, TransferState,
};
use futures::channel::mpsc;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, ClickEvent, Context, ElementId, Global, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::format;
use crate::prompts::{Prompter, TaggedPrompt};
use crate::runtime;
use crate::style::{self, ControlSize, IconButton, TypeScale as _};

/// How often the panel refreshes while something is being copied.
const TICK: Duration = Duration::from_millis(100);

/// Weight of the newest sample in the smoothed transfer rate.
const RATE_SMOOTHING: f64 = 0.3;

/// The transfer queue shared by the window.
pub(crate) struct Transfers {
    queue: TransferQueue,
    resolver: Arc<DialogResolver>,
}

impl Global for Transfers {}

impl Transfers {
    /// Creates the queue; conflicts are asked through `prompts`. Returns the
    /// queue's events.
    pub(crate) fn install(
        prompts: mpsc::UnboundedSender<TaggedPrompt>,
        cx: &mut App,
    ) -> mpsc::UnboundedReceiver<TransferEvent> {
        let (queue, events) = TransferQueue::new(DEFAULT_PARALLEL_FILES);
        cx.set_global(Self {
            queue,
            resolver: Arc::new(DialogResolver {
                prompter: Prompter::new(prompts, None),
            }),
        });
        events
    }

    pub(crate) fn queue(cx: &App) -> TransferQueue {
        cx.global::<Self>().queue.clone()
    }

    /// Starts copying `request`.
    pub(crate) fn start(request: TransferRequest, cx: &App) -> TransferId {
        let transfers = cx.global::<Self>();
        // The queue spawns its work on tokio.
        let _runtime = runtime::handle(cx).enter();
        transfers.queue.enqueue(
            request,
            Arc::clone(&transfers.resolver) as Arc<dyn ConflictResolver>,
        )
    }

    pub(crate) fn pause(id: TransferId, cx: &App) {
        Self::queue(cx).pause(id);
    }

    pub(crate) fn resume(id: TransferId, cx: &App) {
        let _runtime = runtime::handle(cx).enter();
        Self::queue(cx).resume(id);
    }

    pub(crate) fn cancel(id: TransferId, cx: &App) {
        let _runtime = runtime::handle(cx).enter();
        Self::queue(cx).cancel(id);
    }
}

/// Asks the user about conflicts in a dialog.
#[derive(Debug)]
struct DialogResolver {
    prompter: Prompter,
}

#[async_trait]
impl ConflictResolver for DialogResolver {
    async fn resolve(&self, conflict: Conflict) -> Option<ConflictDecision> {
        self.prompter.conflict(conflict).await
    }
}

/// A smoothed transfer rate.
#[derive(Clone, Copy, Debug)]
struct Rate {
    bytes: u64,
    at: Instant,
    per_second: f64,
}

/// The list of transfers at the bottom of the window.
pub(crate) struct TransferPanel {
    snapshots: Vec<TransferSnapshot>,
    rates: HashMap<TransferId, Rate>,
    ticker: Option<Task<()>>,
}

impl TransferPanel {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let mut panel = Self {
            snapshots: Vec::new(),
            rates: HashMap::new(),
            ticker: None,
        };
        panel.refresh(cx);
        panel
    }

    /// Transfers that are working, for the toggle's badge.
    pub(crate) fn active_count(&self) -> usize {
        self.snapshots
            .iter()
            .filter(|snapshot| snapshot.state.is_active())
            .count()
    }

    /// Overall progress of the working transfers, `0.0..=1.0`.
    pub(crate) fn overall_progress(&self) -> Option<f32> {
        let (done, total) = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.state.is_active())
            .fold((0_u64, 0_u64), |(done, total), snapshot| {
                (done + snapshot.bytes_done, total + snapshot.bytes_total)
            });
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "a progress fraction needs no exact bytes"
        )]
        let fraction = (total > 0).then(|| (done as f64 / total as f64) as f32);
        fraction
    }

    /// Re-reads the queue; keeps refreshing while something is running.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.snapshots = Transfers::queue(cx).snapshot();
        self.update_rates();
        if self.active_count() > 0 && self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    let busy = this
                        .update(cx, |panel, cx| {
                            panel.snapshots = Transfers::queue(cx).snapshot();
                            panel.update_rates();
                            cx.notify();
                            panel.active_count() > 0
                        })
                        .unwrap_or(false);
                    if !busy {
                        break;
                    }
                }
                let _ = this.update(cx, |panel, _| panel.ticker = None);
            }));
        }
        cx.notify();
    }

    fn update_rates(&mut self) {
        let now = Instant::now();
        let mut rates = HashMap::with_capacity(self.snapshots.len());
        for snapshot in &self.snapshots {
            let rate = match self.rates.get(&snapshot.id) {
                Some(previous) if snapshot.state == TransferState::Running => {
                    let elapsed = now.duration_since(previous.at).as_secs_f64();
                    if elapsed < 0.05 {
                        *previous
                    } else {
                        #[expect(clippy::cast_precision_loss, reason = "rates are estimates")]
                        let sample =
                            snapshot.bytes_done.saturating_sub(previous.bytes) as f64 / elapsed;
                        Rate {
                            bytes: snapshot.bytes_done,
                            at: now,
                            per_second: if previous.per_second == 0.0 {
                                sample
                            } else {
                                previous.per_second * (1.0 - RATE_SMOOTHING)
                                    + sample * RATE_SMOOTHING
                            },
                        }
                    }
                }
                _ => Rate {
                    bytes: snapshot.bytes_done,
                    at: now,
                    per_second: 0.0,
                },
            };
            rates.insert(snapshot.id, rate);
        }
        self.rates = rates;
    }

    fn render_row(
        &self,
        snapshot: &TransferSnapshot,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let buttons = Self::render_controls(snapshot, cx);
        let theme = cx.theme();
        let (muted, danger, success, accent) = (
            theme.muted_foreground,
            theme.danger,
            theme.success,
            theme.blue,
        );
        let id = snapshot.id;
        let icon = match snapshot.direction {
            Direction::Upload => IconName::ArrowUp,
            Direction::Download => IconName::ArrowDown,
            Direction::Copy => IconName::ArrowRight,
        };
        let percent = percent(snapshot);
        let status = self.status_text(snapshot);
        let state_icon = match snapshot.state {
            TransferState::Completed => Some(Icon::new(IconName::CircleCheck).text_color(success)),
            TransferState::Failed => Some(Icon::new(IconName::CircleAlert).text_color(danger)),
            _ => None,
        };
        let error = first_error(snapshot)
            .map(|text| div().text_small().text_color(danger).truncate().child(text));
        let bar_color = match snapshot.state {
            TransferState::Completed => success,
            TransferState::Failed => danger,
            TransferState::Paused | TransferState::Cancelled => muted,
            _ => accent,
        };
        v_flex()
            .id(ElementId::NamedInteger("transfer".into(), id_number(id)))
            .px_3()
            .py_2()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(icon).size_4().text_color(muted))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_body()
                            .child(SharedString::from(snapshot.title.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_small()
                            .text_color(muted)
                            .child(SharedString::from(format!(
                                "{} → {}:{}",
                                snapshot.source_label, snapshot.target_label, snapshot.target_dir
                            ))),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_small()
                            .text_color(muted)
                            .child(status),
                    )
                    .children(state_icon)
                    .child(buttons),
            )
            // The bar spans the whole row so a finished transfer reads as full.
            .child(style::progress(
                ElementId::NamedInteger("progress".into(), id_number(id)),
                percent,
                bar_color,
            ))
            .children(error)
    }

    /// Pause, resume, retry, cancel and remove, as fits the state.
    fn render_controls(snapshot: &TransferSnapshot, cx: &mut Context<Self>) -> gpui::Div {
        let id = snapshot.id;
        let control = |name: &'static str, icon: IconName, tooltip: &'static str| {
            IconButton::new(ElementId::NamedInteger(name.into(), id_number(id)), icon)
                .size(ControlSize::Compact)
                .tooltip(tooltip)
        };
        h_flex()
            .gap_1()
            .flex_shrink_0()
            .when(snapshot.state.is_active(), |this| {
                this.child(control("pause", IconName::Pause, "Pause").on_click(
                    cx.listener(move |_, _: &ClickEvent, _, cx| Transfers::pause(id, cx)),
                ))
            })
            .when(snapshot.state == TransferState::Paused, |this| {
                this.child(
                    control("resume", IconName::Play, "Resume").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            Transfers::resume(id, cx);
                            this.refresh(cx);
                        },
                    )),
                )
            })
            .when(snapshot.state == TransferState::Failed, |this| {
                this.child(
                    control("retry", IconName::RefreshCw, "Retry").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            Transfers::resume(id, cx);
                            this.refresh(cx);
                        },
                    )),
                )
            })
            .map(|this| {
                if matches!(
                    snapshot.state,
                    TransferState::Completed | TransferState::Failed | TransferState::Cancelled
                ) {
                    this.child(
                        control("remove", IconName::Close, "Remove from list").on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                Transfers::queue(cx).remove(id);
                                this.refresh(cx);
                            }),
                        ),
                    )
                } else {
                    this.child(control("cancel", IconName::Close, "Cancel").on_click(
                        cx.listener(move |_, _: &ClickEvent, _, cx| Transfers::cancel(id, cx)),
                    ))
                }
            })
    }

    fn status_text(&self, snapshot: &TransferSnapshot) -> SharedString {
        let sizes = format!(
            "{} of {}",
            format::size(snapshot.bytes_done),
            format::size(snapshot.bytes_total)
        );
        let files = if snapshot.files_total > 1 {
            format!(
                " · {} / {} files",
                format::count(usize::try_from(snapshot.files_done).unwrap_or(usize::MAX)),
                format::count(usize::try_from(snapshot.files_total).unwrap_or(usize::MAX))
            )
        } else {
            String::new()
        };
        let text = match snapshot.state {
            TransferState::Waiting => "Waiting…".to_owned(),
            TransferState::Scanning => "Looking for files…".to_owned(),
            TransferState::Running => {
                let rate = self
                    .rates
                    .get(&snapshot.id)
                    .map_or(0.0, |rate| rate.per_second);
                if rate > 1.0 {
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        clippy::cast_precision_loss,
                        reason = "display only; rates are positive and far below u64::MAX"
                    )]
                    let (per_second, left) = (
                        rate as u64,
                        snapshot.bytes_total.saturating_sub(snapshot.bytes_done) as f64 / rate,
                    );
                    format!(
                        "{sizes}{files} · {}/s · {}",
                        format::size(per_second),
                        remaining(left)
                    )
                } else {
                    format!("{sizes}{files}")
                }
            }
            TransferState::Paused => format!("Paused · {sizes}{files}"),
            TransferState::Completed => {
                let skipped = if snapshot.files_skipped > 0 {
                    format!(" · {} skipped", snapshot.files_skipped)
                } else {
                    String::new()
                };
                format!(
                    "Done · {}{files}{skipped}",
                    format::size(snapshot.bytes_total)
                )
            }
            TransferState::Failed => {
                format!("{} failed · {sizes}{files}", snapshot.files_failed.max(1))
            }
            TransferState::Cancelled => "Cancelled".to_owned(),
        };
        text.into()
    }
}

impl Render for TransferPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, background) = (theme.muted_foreground, theme.background);
        let active = self.active_count();
        let finished = self.snapshots.iter().any(|snapshot| {
            matches!(
                snapshot.state,
                TransferState::Completed | TransferState::Cancelled
            )
        });
        let rows: Vec<_> = self
            .snapshots
            .iter()
            .rev()
            .enumerate()
            .map(|(index, snapshot)| style::divided(self.render_row(snapshot, cx), index, cx))
            .collect();
        let empty = rows.is_empty().then(|| {
            div().px_1().text_small().text_color(muted).child(
                "Nothing transferred yet. Copy with Ctrl+C and Ctrl+V, with F5 between two \
                 panes, or drop files onto a folder.",
            )
        });
        v_flex()
            .size_full()
            .bg(background)
            .child(
                h_flex()
                    .h(style::HEADER_HEIGHT)
                    .px(style::BAR_PADDING)
                    .gap_2()
                    .flex_shrink_0()
                    .child(div().text_title().child("Transfers"))
                    .when(active > 0, |this| {
                        this.child(
                            div()
                                .text_small()
                                .text_color(muted)
                                .child(format!("{active} running")),
                        )
                    })
                    .child(div().flex_1())
                    .when(finished, |this| {
                        this.child(
                            Button::new("clear-finished")
                                .ghost()
                                .small()
                                .label("Clear finished")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    Transfers::queue(cx).clear_finished();
                                    this.refresh(cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("transfer-list")
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .pb_3()
                    .overflow_y_scroll()
                    .children(empty)
                    .when(!rows.is_empty(), |this| {
                        this.child(style::group(cx).children(rows))
                    }),
            )
    }
}

/// "12 s left", "3 min left", "2 h left".
fn remaining(seconds: f64) -> String {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "display only; the value is a positive estimate"
    )]
    let seconds = seconds.max(0.0).ceil() as u64;
    match seconds {
        0..60 => format!("{seconds} s left"),
        60..3_600 => format!("{} min left", seconds.div_ceil(60)),
        _ => format!("{} h {} min left", seconds / 3_600, (seconds % 3_600) / 60),
    }
}

fn id_number(id: TransferId) -> u64 {
    id.get()
}

/// Completed share of a transfer, in percent.
fn percent(snapshot: &TransferSnapshot) -> f32 {
    if snapshot.bytes_total == 0 {
        return if snapshot.state == TransferState::Completed {
            100.0
        } else {
            0.0
        };
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a progress bar needs no exact bytes"
    )]
    let percent = (snapshot.bytes_done as f64 / snapshot.bytes_total as f64 * 100.0) as f32;
    percent
}

/// The first error and how many more there are.
fn first_error(snapshot: &TransferSnapshot) -> Option<SharedString> {
    let first = snapshot.errors.first()?;
    let more = snapshot.errors.len() - 1;
    Some(if more > 0 {
        format!("{first} (+{more} more)").into()
    } else {
        first.clone().into()
    })
}
