//! The window's transfer queue and the panel that shows it.
//!
//! The queue lives in a GPUI global so any view (a file list receiving a
//! drop, a paste) can start a transfer. Conflicts are asked through the
//! window's dialogs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ferge_core::transfer::direct;
use ferge_core::transfer::route::{QueueOptions, Route};
use ferge_core::transfer::{
    Conflict, ConflictDecision, ConflictResolver, DEFAULT_PARALLEL_FILES, DirectDecision,
    DirectRequest, Direction, TransferEvent, TransferId, TransferQueue, TransferRequest,
    TransferSnapshot, TransferState,
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
use crate::reconnect::{Connector, worth_retrying};
use crate::runtime;
use crate::style::{self, ControlSize, IconButton, TypeScale as _};

/// How often the panel refreshes while something is being copied.
const TICK: Duration = Duration::from_millis(100);

/// Weight of the newest sample in the smoothed transfer rate.
const RATE_SMOOTHING: f64 = 0.3;

/// Transfer traces kept; older ones are deleted at startup.
const TRACES_KEPT: usize = 200;

/// The transfer queue shared by the window.
pub(crate) struct Transfers {
    queue: TransferQueue,
    resolver: Arc<DialogResolver>,
    /// Where running direct jobs and saved transfers are kept, to go on
    /// with them after a restart.
    records: Option<PathBuf>,
    /// Connects for transfers once the window is there.
    connector: Option<Connector>,
    /// Transfers ferge is reconnecting after a lost connection.
    reconnecting: Arc<Mutex<HashSet<TransferId>>>,
}

/// How often and how long ferge tries to connect again after a lost
/// connection.
const RECONNECT_EVERY: Duration = Duration::from_secs(5);
const RECONNECT_ATTEMPTS: usize = 360;

/// What to do once a transfer is connected again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    Resume,
    Cancel,
}

impl Global for Transfers {}

impl Transfers {
    /// Creates the queue; conflicts are asked through `prompts`. Returns the
    /// queue's events.
    /// `ephemeral` (benchmarks) records no traces unless `FERGE_TRACE_DIR`
    /// asks for them.
    pub(crate) fn install(
        prompts: mpsc::UnboundedSender<TaggedPrompt>,
        ephemeral: bool,
        cx: &mut App,
    ) -> mpsc::UnboundedReceiver<TransferEvent> {
        let mut options = QueueOptions::new(DEFAULT_PARALLEL_FILES);
        if !ephemeral {
            let state = crate::logging::state_dir();
            options.trace_dir = state.as_ref().map(|dir| dir.join("transfers"));
            options.direct.records = state.map(|dir| dir.join("jobs"));
        }
        let options = options.with_env();
        let records = options.direct.records.clone();
        if let Some(dir) = &options.trace_dir {
            tracing::info!(dir = %dir.display(), "recording transfer traces");
            // Old traces go on the I/O runtime, never on the UI thread.
            let dir = dir.clone();
            runtime::handle(cx).spawn(ferge_core::transfer::trace::prune(dir, TRACES_KEPT));
        }
        let (queue, events) = TransferQueue::with_options(options);
        cx.set_global(Self {
            queue,
            resolver: Arc::new(DialogResolver {
                prompter: Prompter::new(prompts, None),
                direct_answers: Mutex::new(HashMap::new()),
            }),
            records,
            connector: None,
            reconnecting: Arc::default(),
        });
        events
    }

    /// Lets transfers connect on their own: to follow direct jobs again, to
    /// resume saved transfers and after lost connections. Then brings back
    /// what ran when ferge quit.
    pub(crate) fn start_connecting(connector: Connector, cx: &mut App) {
        cx.global_mut::<Self>().connector = Some(connector);
        Self::follow_recorded(cx);
        Self::restore_saved(cx);
    }

    /// Shows the transfers saved when ferge quit, paused; they connect when
    /// resumed.
    fn restore_saved(cx: &App) {
        let transfers = cx.global::<Self>();
        let Some(dir) = transfers.records.clone() else {
            return;
        };
        let queue = transfers.queue.clone();
        let resolver = Arc::clone(&transfers.resolver) as Arc<dyn ConflictResolver>;
        runtime::spawn(cx, async move {
            for saved in ferge_core::transfer::saved_transfers(&dir).await {
                tracing::info!(title = %saved.title, "restoring a transfer saved when ferge quit");
                queue.restore(&saved, Arc::clone(&resolver));
            }
        });
    }

    /// Connects a transfer's two sides again, then resumes or cancels it.
    /// After a lost connection (`keep_trying`), tries for a while as long as
    /// the network or the server is missing.
    fn reconnect(id: TransferId, then: Then, keep_trying: bool, cx: &App) {
        let transfers = cx.global::<Self>();
        let Some(connector) = transfers.connector.clone() else {
            return;
        };
        let queue = transfers.queue.clone();
        let reconnecting = Arc::clone(&transfers.reconnecting);
        if !reconnecting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id)
        {
            return;
        }
        runtime::spawn(cx, async move {
            let attempts = if keep_trying { RECONNECT_ATTEMPTS } else { 1 };
            let mut outcome = None;
            for attempt in 0..attempts {
                let Some(snapshot) = queue.snapshot().into_iter().find(|s| s.id == id) else {
                    break;
                };
                // Resumed, cancelled or removed meanwhile.
                if keep_trying && !snapshot.lost_connection {
                    break;
                }
                if attempt > 0 {
                    tokio::time::sleep(RECONNECT_EVERY).await;
                }
                match connector
                    .connect_both(
                        snapshot.source_server.as_ref(),
                        snapshot.target_server.as_ref(),
                    )
                    .await
                {
                    Ok((source, target)) => {
                        outcome = Some(Ok(queue.reconnect(id, source, target)));
                        break;
                    }
                    Err(error) if keep_trying && worth_retrying(&error) => {
                        tracing::debug!(transfer = ?id, %error, "not connected yet");
                    }
                    Err(error) => {
                        outcome = Some(Err(error));
                        break;
                    }
                }
            }
            reconnecting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            match (outcome, then) {
                (Some(Ok(_)), Then::Resume) => queue.resume(id),
                (Some(Ok(_)), Then::Cancel) => queue.cancel(id),
                (Some(Err(error)), Then::Cancel) => {
                    // Cancel all the same; partial files stay on the server.
                    queue.cancel(id);
                    connector.notice(format!(
                        "The transfer was cancelled, but ferge could not connect to remove \
                         its partial files: {error}"
                    ));
                }
                (Some(Err(error)), Then::Resume) => connector.notice(format!(
                    "ferge could not connect to resume the transfer: {error}"
                )),
                (None, _) => {}
            }
        });
    }

    /// Whether ferge is connecting a transfer again right now.
    fn is_reconnecting(id: TransferId, cx: &App) -> bool {
        cx.global::<Self>()
            .reconnecting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id)
    }

    /// A transfer lost its connection: go on once it is back.
    pub(crate) fn connection_lost(id: TransferId, cx: &App) {
        Self::reconnect(id, Then::Resume, true, cx);
    }

    /// The transfers that stop when ferge quits: running, not directly
    /// between servers.
    pub(crate) fn needing_ferge(cx: &App) -> Vec<TransferId> {
        Self::queue(cx)
            .snapshot()
            .into_iter()
            .filter(|snapshot| {
                snapshot.state.is_active() && !snapshot.route.is_some_and(Route::is_direct)
            })
            .map(|snapshot| snapshot.id)
            .collect()
    }

    /// Follows the direct jobs again that ran when ferge quit: connects to
    /// both servers of each in the background (asking for passwords like any
    /// connection) and shows the job in the list until it ends. A job whose
    /// servers cannot be reached stays recorded for the next start.
    fn follow_recorded(cx: &App) {
        let transfers = cx.global::<Self>();
        let (Some(dir), Some(connector)) = (transfers.records.clone(), transfers.connector.clone())
        else {
            return;
        };
        let queue = transfers.queue.clone();
        runtime::spawn(cx, async move {
            for record in direct::recorded_jobs(&dir).await {
                tracing::info!(job = %record.job, title = %record.title, "following a direct transfer again");
                let side = |server: &direct::RecordedServer| {
                    ferge_core::transfer::SavedSide::Server(server.clone()).destination()
                };
                let (source, target) = (side(&record.source), side(&record.target));
                match connector
                    .connect_both(source.as_ref(), target.as_ref())
                    .await
                {
                    Ok((source, target)) => {
                        queue.attach(record, source, target);
                    }
                    Err(error) => {
                        tracing::warn!(job = %record.job, %error, "cannot follow a direct transfer again");
                        connector.notice(format!(
                            "ferge could not reconnect to follow “{}” ({} → {}): {}. \
                             It goes on on the servers; ferge tries again at the next start.",
                            format::shown(&record.title),
                            record.source.label,
                            record.target.label,
                            error,
                        ));
                    }
                }
            }
        });
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

    /// Resumes a transfer, connecting it first if it has no connections (it
    /// was restored, or lost them).
    pub(crate) fn resume(id: TransferId, cx: &App) {
        let snapshot = Self::queue(cx).snapshot().into_iter().find(|s| s.id == id);
        match snapshot {
            Some(snapshot) if !snapshot.connected || snapshot.lost_connection => {
                Self::reconnect(id, Then::Resume, false, cx);
            }
            _ => {
                let _runtime = runtime::handle(cx).enter();
                Self::queue(cx).resume(id);
            }
        }
    }

    /// Cancels a transfer; one without connections connects first, to
    /// remove its partial files.
    pub(crate) fn cancel(id: TransferId, cx: &App) {
        let snapshot = Self::queue(cx).snapshot().into_iter().find(|s| s.id == id);
        match snapshot {
            Some(snapshot)
                if snapshot.state == TransferState::Paused
                    && (!snapshot.connected || snapshot.lost_connection) =>
            {
                Self::reconnect(id, Then::Cancel, false, cx);
            }
            _ => {
                let _runtime = runtime::handle(cx).enter();
                Self::queue(cx).cancel(id);
            }
        }
    }
}

/// Asks the user about conflicts and direct copies in a dialog.
#[derive(Debug)]
struct DialogResolver {
    prompter: Prompter,
    /// Answers about direct copies to keep until ferge quits, by the pair of
    /// servers (in either direction).
    direct_answers: Mutex<HashMap<(String, String), bool>>,
}

impl DialogResolver {
    fn pair(request: &DirectRequest) -> (String, String) {
        let mut pair = [request.source_id.clone(), request.target_id.clone()];
        pair.sort();
        let [first, second] = pair;
        (first, second)
    }
}

#[async_trait]
impl ConflictResolver for DialogResolver {
    async fn resolve(&self, conflict: Conflict) -> Option<ConflictDecision> {
        self.prompter.conflict(conflict).await
    }

    async fn approve_direct(&self, request: DirectRequest) -> DirectDecision {
        let pair = Self::pair(&request);
        let known = self
            .direct_answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&pair)
            .copied();
        let allow = if let Some(allow) = known {
            allow
        } else {
            let Some(answer) = self.prompter.direct(request).await else {
                return DirectDecision::Relay;
            };
            if answer.remember {
                self.direct_answers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(pair, answer.allow);
            }
            answer.allow
        };
        if allow {
            DirectDecision::Allow
        } else {
            DirectDecision::Relay
        }
    }
}

/// Asks the window to open a transfer's source or target folder in a new
/// tab.
#[derive(Clone, Debug)]
pub(crate) struct OpenPlace(pub(crate) ferge_core::destination::Destination);

impl gpui::EventEmitter<OpenPlace> for TransferPanel {}

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
        let status = self.status_text(snapshot, cx);
        let state_icon = match snapshot.state {
            TransferState::Completed => Some(Icon::new(IconName::CircleCheck).text_color(success)),
            TransferState::Failed => Some(Icon::new(IconName::CircleAlert).text_color(danger)),
            _ => None,
        };
        let error = first_error(snapshot)
            .map(|text| div().text_small().text_color(danger).truncate().child(text));
        // Why the data takes the long way, while that is worth knowing.
        let route_note = snapshot
            .route_note
            .clone()
            .filter(|_| snapshot.route == Some(Route::Relay) && !snapshot.state.is_settled())
            .map(|note| {
                div()
                    .text_small()
                    .text_color(muted)
                    .truncate()
                    .child(SharedString::from(format!("Not direct: {note}")))
            });
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
                            .child(SharedString::from(format::shown(&snapshot.title))),
                    )
                    .child(Self::render_places(snapshot, cx))
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
            .children(route_note)
    }

    /// "source → target:folder", each side a link that opens its folder in a
    /// new tab, and the route.
    fn render_places(snapshot: &TransferSnapshot, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted, foreground) = (theme.muted_foreground, theme.foreground);
        let id = id_number(snapshot.id);
        let link = |name: &'static str,
                    text: String,
                    place: Option<ferge_core::destination::Destination>,
                    cx: &mut Context<Self>| {
            let element = div()
                .id(ElementId::NamedInteger(name.into(), id))
                .min_w_0()
                .truncate()
                .child(SharedString::from(format::shown(&text)));
            match place {
                Some(place) => element
                    .cursor_pointer()
                    .hover(move |style| style.text_color(foreground))
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Open this folder in a new tab")
                            .build(window, cx)
                    })
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.emit(OpenPlace(place.clone()));
                    })),
                None => element,
            }
        };
        h_flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_small()
            .text_color(muted)
            .child(link(
                "transfer-source",
                snapshot.source_label.clone(),
                snapshot.source_place.clone(),
                cx,
            ))
            .child(div().flex_shrink_0().px(style::em(0.3)).child("→"))
            .child(link(
                "transfer-target",
                format!("{}:{}", snapshot.target_label, snapshot.target_dir),
                snapshot.target_place.clone(),
                cx,
            ))
            .child(div().min_w_0().truncate().child(route_label(snapshot)))
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
            .when(snapshot.state.is_active() && !snapshot.attached, |this| {
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

    fn status_text(&self, snapshot: &TransferSnapshot, cx: &App) -> SharedString {
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
            TransferState::Paused if Transfers::is_reconnecting(snapshot.id, cx) => {
                format!("Connecting again… · {sizes}{files}")
            }
            TransferState::Paused if snapshot.lost_connection => {
                format!("Connection lost · {sizes}{files}")
            }
            TransferState::Paused if !snapshot.connected => {
                format!("Paused when ferge quit · {sizes}{files}")
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
    // Errors name paths and quote the server, so they are shown as names are.
    let first = crate::format::shown(snapshot.errors.first()?);
    let more = snapshot.errors.len() - 1;
    Some(if more > 0 {
        format!("{first} (+{more} more)").into()
    } else {
        first.into()
    })
}

/// How the data travels, where more than one way was possible.
fn route_label(snapshot: &TransferSnapshot) -> &'static str {
    match snapshot.route {
        Some(Route::DirectPush | Route::DirectPull) if snapshot.attached => {
            " · directly between the servers, followed again after a restart"
        }
        Some(Route::DirectPush | Route::DirectPull) => " · directly between the servers",
        Some(Route::Server) => " · on the server",
        // A direct transfer was tried and did not work out.
        Some(Route::Relay) if snapshot.route_note.is_some() => " · through this computer",
        _ => "",
    }
}
