//! sendme balloon: a tiny desktop companion for sendme.
//!
//! Shows a little balloon hovering on the desktop (frameless, transparent
//! window, Wayland/XWayland compatible). The upper half of the balloon sends a file,
//! the lower half receives one.
//!
//! - Click the upper (blue) half: a file dialog opens, the chosen file is
//!   imported and a ticket is shown, with a button to copy the
//!   `sendme receive <ticket>` command to the clipboard. The balloon waits
//!   until the file was transferred or you press cancel.
//! - Click the lower (green) half: paste a ticket, choose where to save,
//!   and the data is downloaded to that location.

use std::{
    path::PathBuf,
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, Pos2, Rect, RichText, Sense,
    Shape, Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use indicatif::HumanBytes;
use sendme::balloon::{parse_ticket, receive_ticket, send_file, ReceiveEvent, SendEvent};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11;

/// Commands from the GUI to the background worker.
enum Command {
    PickAndSend,
    SendPath(PathBuf),
    CancelSend,
    Receive { ticket: String },
    CancelReceive,
}

/// Events from the background worker to the GUI.
enum UiEvent {
    FilePickCancelled,
    SendStarted { name: String },
    Send(SendEvent),
    TicketInvalid(String),
    FolderPickCancelled,
    ReceiveStarting,
    Receive(ReceiveEvent),
}

#[derive(Clone)]
enum UiState {
    /// The plain balloon, waiting for a click.
    Idle,
    /// The file open dialog is showing.
    PickingFile,
    /// The chosen file is being imported into the blob store.
    Preparing { name: String },
    /// Ticket ready, waiting for a peer to fetch the data.
    Waiting {
        ticket: String,
        name: String,
        size: u64,
        peer: bool,
        sent: u64,
    },
    /// The data was transferred to a peer.
    SendDone { name: String },
    /// Asking the user to paste a ticket.
    EnterTicket { error: Option<String> },
    /// The folder picker dialog is showing.
    PickingFolder,
    /// Download in progress.
    Receiving {
        status: String,
        current: u64,
        total: u64,
    },
    /// Download finished and saved.
    ReceiveDone { target: PathBuf },
    /// Something went wrong.
    Error { message: String },
}

const SEND_COLOR: Color32 = Color32::from_rgb(66, 133, 244);
const SEND_COLOR_HOVER: Color32 = Color32::from_rgb(108, 160, 247);
const RECV_COLOR: Color32 = Color32::from_rgb(52, 168, 83);
const RECV_COLOR_HOVER: Color32 = Color32::from_rgb(94, 190, 120);
const BUBBLE_BG: Color32 = Color32::from_rgb(32, 33, 36);
const IDLE_SIZE: Vec2 = Vec2::new(164.0, 150.0);

struct BalloonApp {
    state: UiState,
    ticket_text: String,
    copied_at: Option<Instant>,
    last_size: Vec2,
    cmd_tx: tokio_mpsc::UnboundedSender<Command>,
    evt_rx: std_mpsc::Receiver<UiEvent>,
}

impl BalloonApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std_mpsc::channel();
        spawn_worker(cc.egui_ctx.clone(), cmd_rx, evt_tx);
        Self {
            state: UiState::Idle,
            ticket_text: String::new(),
            copied_at: None,
            last_size: Vec2::ZERO,
            cmd_tx,
            evt_rx,
        }
    }

    fn send_cmd(&self, cmd: Command) {
        self.cmd_tx.send(cmd).ok();
    }

    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::FilePickCancelled => {
                if matches!(self.state, UiState::PickingFile) {
                    self.state = UiState::Idle;
                }
            }
            UiEvent::SendStarted { name } => {
                self.state = UiState::Preparing { name };
            }
            UiEvent::Send(e) => match (e, &mut self.state) {
                (
                    SendEvent::TicketReady { ticket, name, size },
                    UiState::Preparing { .. } | UiState::PickingFile,
                ) => {
                    self.state = UiState::Waiting {
                        ticket,
                        name,
                        size,
                        peer: false,
                        sent: 0,
                    };
                }
                (SendEvent::PeerConnected, UiState::Waiting { peer, .. }) => {
                    *peer = true;
                }
                (SendEvent::Progress { sent }, UiState::Waiting { sent: s, peer, .. }) => {
                    *peer = true;
                    *s = sent;
                }
                (SendEvent::Completed, UiState::Waiting { name, .. }) => {
                    let name = name.clone();
                    self.state = UiState::SendDone { name };
                }
                (SendEvent::Error(message), UiState::Preparing { .. } | UiState::Waiting { .. }) => {
                    self.state = UiState::Error { message };
                }
                _ => {}
            },
            UiEvent::TicketInvalid(message) => {
                self.state = UiState::EnterTicket {
                    error: Some(message),
                };
            }
            UiEvent::FolderPickCancelled => {
                if matches!(self.state, UiState::PickingFolder) {
                    self.state = UiState::EnterTicket { error: None };
                }
            }
            UiEvent::ReceiveStarting => {
                self.state = UiState::Receiving {
                    status: "connecting ...".into(),
                    current: 0,
                    total: 0,
                };
            }
            UiEvent::Receive(e) => match (e, &mut self.state) {
                (ReceiveEvent::Connecting, UiState::Receiving { status, .. }) => {
                    *status = "connecting ...".into();
                }
                (
                    ReceiveEvent::Starting {
                        total_files,
                        payload_size,
                    },
                    UiState::Receiving { status, .. },
                ) => {
                    *status = format!(
                        "downloading {} file(s), {}",
                        total_files,
                        HumanBytes(payload_size)
                    );
                }
                (
                    ReceiveEvent::Progress { current, total },
                    UiState::Receiving {
                        current: c,
                        total: t,
                        ..
                    },
                ) => {
                    *c = current;
                    *t = total;
                }
                (ReceiveEvent::Exporting, UiState::Receiving { status, .. }) => {
                    *status = "saving files ...".into();
                }
                (ReceiveEvent::Completed { target }, UiState::Receiving { .. }) => {
                    self.state = UiState::ReceiveDone { target };
                }
                (
                    ReceiveEvent::Error(message),
                    UiState::Receiving { .. } | UiState::PickingFolder,
                ) => {
                    self.state = UiState::Error { message };
                }
                _ => {}
            },
        }
    }

    fn desired_size(&self) -> Vec2 {
        match self.state {
            UiState::Idle => IDLE_SIZE,
            _ => Vec2::new(370.0, 250.0),
        }
    }

    /// The idle balloon: top half sends, bottom half receives.
    fn draw_balloon(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (rect, resp) = ui.allocate_exact_size(IDLE_SIZE, Sense::click_and_drag());
        let center = Pos2::new(rect.center().x, rect.top() + 72.0);
        let r = 60.0;
        let in_circle = |p: Pos2| center.distance(p) <= r;
        let hover = resp.hover_pos();
        let hover_top = hover
            .map(|p| in_circle(p) && p.y < center.y)
            .unwrap_or(false);
        let hover_bottom = hover
            .map(|p| in_circle(p) && p.y >= center.y)
            .unwrap_or(false);
        let files_hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        // Native file-drop events do not include coordinates, so the upper half
        // is the drop target whenever a file is over this compact window.
        let drop_hover = files_hovering;
        let send_col = if hover_top {
            SEND_COLOR_HOVER
        } else {
            SEND_COLOR
        };
        let recv_col = if hover_bottom {
            RECV_COLOR_HOVER
        } else {
            RECV_COLOR
        };
        let painter = ui.painter();

        painter.circle_filled(
            center + Vec2::new(0.0, 3.0),
            r + 2.0,
            Color32::from_black_alpha(90),
        );

        // A small knot keeps the silhouette recognizable without wasting space on a string.
        let knot_tip = Pos2::new(center.x, center.y + r + 9.0);
        let knot = vec![
            Pos2::new(center.x - 7.0, center.y + r - 3.0),
            Pos2::new(center.x + 7.0, center.y + r - 3.0),
            knot_tip,
        ];
        painter.add(Shape::convex_polygon(
            knot,
            recv_col,
            Stroke::new(1.5, Color32::from_gray(40)),
        ));

        // two visually distinct halves
        let top_half = Rect::from_min_max(
            Pos2::new(rect.left(), center.y - r - 3.0),
            Pos2::new(rect.right(), center.y),
        );
        let bottom_half = Rect::from_min_max(
            Pos2::new(rect.left(), center.y),
            Pos2::new(rect.right(), center.y + r + 3.0),
        );
        painter
            .with_clip_rect(top_half)
            .circle_filled(center, r, send_col);
        painter
            .with_clip_rect(bottom_half)
            .circle_filled(center, r, recv_col);
        if drop_hover {
            painter.with_clip_rect(top_half).circle_stroke(
                center,
                r - 4.0,
                Stroke::new(3.0, Color32::WHITE),
            );
        }
        painter.line_segment(
            [
                Pos2::new(center.x - r, center.y),
                Pos2::new(center.x + r, center.y),
            ],
            Stroke::new(1.0, Color32::from_white_alpha(180)),
        );
        painter.circle_stroke(center, r, Stroke::new(2.0, Color32::from_gray(35)));
        painter.circle_filled(
            Pos2::new(center.x - r * 0.38, center.y - r * 0.48),
            5.0,
            Color32::from_white_alpha(65),
        );

        // labels
        painter.text(
            Pos2::new(center.x, center.y - 34.0),
            Align2::CENTER_CENTER,
            if drop_hover { "Drop to send" } else { "Send" },
            FontId::proportional(16.0),
            Color32::WHITE,
        );
        painter.text(
            Pos2::new(center.x, center.y - 17.0),
            Align2::CENTER_CENTER,
            if drop_hover {
                "release file"
            } else {
                "drop file or click"
            },
            FontId::proportional(10.0),
            Color32::from_white_alpha(210),
        );
        painter.text(
            Pos2::new(center.x, center.y + 27.0),
            Align2::CENTER_CENTER,
            "Receive",
            FontId::proportional(16.0),
            Color32::WHITE,
        );
        painter.text(
            Pos2::new(center.x, center.y + 44.0),
            Align2::CENTER_CENTER,
            "paste a ticket",
            FontId::proportional(10.0),
            Color32::from_white_alpha(210),
        );

        // small close button, top right
        let close_rect = Rect::from_min_size(
            Pos2::new(rect.right() - 24.0, rect.top() + 2.0),
            Vec2::splat(20.0),
        );
        let close_resp = ui.interact(close_rect, ui.id().with("close"), Sense::click());
        let close_col = if close_resp.hovered() {
            Color32::WHITE
        } else {
            Color32::from_gray(140)
        };
        ui.painter().text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "✕",
            FontId::proportional(15.0),
            close_col,
        );
        if close_resp.clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let dropped_path = ctx.input(|i| first_dropped_path(&i.raw.dropped_files));
        if let Some(path) = dropped_path {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.state = UiState::Preparing { name };
            self.send_cmd(Command::SendPath(path));
            return;
        }

        // interactions: drag moves the window, click triggers an action
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                if in_circle(p) {
                    if p.y < center.y {
                        self.state = UiState::PickingFile;
                        self.send_cmd(Command::PickAndSend);
                    } else {
                        self.state = UiState::EnterTicket { error: None };
                    }
                }
            }
        }
    }

    /// Title bar of the bubble, acts as drag handle and shows a close button.
    fn title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, title: &str) {
        let resp = ui
            .horizontal(|ui| {
                ui.label(RichText::new(title).strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕").on_hover_text("quit").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                });
            })
            .response;
        let drag = ui.interact(
            resp.rect.shrink2(Vec2::new(40.0, 0.0)),
            ui.id().with("drag"),
            Sense::drag(),
        );
        if drag.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        ui.separator();
    }

    fn draw_state(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let state = self.state.clone();
        match state {
            UiState::Idle => unreachable!(),
            UiState::PickingFile => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("choose a file in the dialog ...");
                });
            }
            UiState::Preparing { name } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("preparing \"{name}\" ..."));
                });
            }
            UiState::Waiting {
                ticket,
                name,
                size,
                peer,
                sent,
            } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.label(format!("{name} ({})", HumanBytes(size)));
                ui.add_space(4.0);
                ui.label("Ticket for the other side:");
                egui::ScrollArea::vertical().max_height(60.0).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&ticket).monospace().size(10.0),
                        )
                        .wrap(),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("📋 Copy ticket").clicked() {
                        ctx.copy_text(ticket.clone());
                        self.copied_at = Some(Instant::now());
                    }
                    if self
                        .copied_at
                        .map(|t| t.elapsed() < Duration::from_secs(2))
                        .unwrap_or(false)
                    {
                        ui.colored_label(RECV_COLOR, "✓ copied");
                    }
                    if ui.button("Cancel").clicked() {
                        self.send_cmd(Command::CancelSend);
                        self.state = UiState::Idle;
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    if peer {
                        ui.label("peer connected, sending ...");
                    } else {
                        ui.label("waiting for the receiver ...");
                    }
                });
                if peer && size > 0 {
                    let sent = sent.min(size);
                    let frac = sent as f32 / size as f32;
                    ui.add(egui::ProgressBar::new(frac).text(format!(
                        "{} / {}",
                        HumanBytes(sent),
                        HumanBytes(size)
                    )));
                }
            }
            UiState::SendDone { name } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.colored_label(RECV_COLOR, format!("✓ \"{name}\" was transferred."));
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
            UiState::EnterTicket { error } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.label("Paste the ticket (or the whole \"sendme receive ...\" command):");
                ui.add(
                    egui::TextEdit::multiline(&mut self.ticket_text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Ctrl+V to paste"),
                );
                if let Some(err) = &error {
                    ui.colored_label(Color32::from_rgb(230, 90, 90), format!("invalid ticket: {err}"));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let ok = ui.add_enabled(
                        !self.ticket_text.trim().is_empty(),
                        egui::Button::new("Receive"),
                    );
                    if ok.clicked() {
                        self.state = UiState::PickingFolder;
                        self.send_cmd(Command::Receive {
                            ticket: self.ticket_text.clone(),
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        self.state = UiState::Idle;
                    }
                });
            }
            UiState::PickingFolder => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("choose where to save in the dialog ...");
                });
            }
            UiState::Receiving {
                status,
                current,
                total,
            } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(&status);
                });
                ui.add_space(6.0);
                if total > 0 {
                    let frac = current as f32 / total as f32;
                    ui.add(egui::ProgressBar::new(frac).text(format!(
                        "{} / {}",
                        HumanBytes(current),
                        HumanBytes(total)
                    )));
                }
                ui.add_space(6.0);
                if ui.button("Cancel").clicked() {
                    self.send_cmd(Command::CancelReceive);
                    self.state = UiState::Idle;
                }
            }
            UiState::ReceiveDone { target } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.add_space(8.0);
                ui.colored_label(
                    RECV_COLOR,
                    format!("✓ saved to {}", target.display()),
                );
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
            UiState::Error { message } => {
                self.title_bar(ui, ctx, "🎈 sendme");
                ui.add_space(8.0);
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("error: {message}"));
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
        }
    }
}

impl eframe::App for BalloonApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // fully transparent background, only the balloon/bubble is visible
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // apply events from the worker
        while let Ok(event) = self.evt_rx.try_recv() {
            self.apply(event);
        }
        // keep spinners animated while something is going on
        if !matches!(self.state, UiState::Idle) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        // resize the window to fit the current state
        let desired = self.desired_size();
        if self.last_size != desired {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(desired));
            self.last_size = desired;
        }

        let frame = if matches!(self.state, UiState::Idle) {
            Frame::NONE
        } else {
            Frame::new()
                .fill(BUBBLE_BG)
                .corner_radius(CornerRadius::same(14))
                .stroke(Stroke::new(1.0, Color32::from_gray(80)))
                .inner_margin(Margin::same(12))
        };
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if matches!(self.state, UiState::Idle) {
                self.draw_balloon(ui, &ctx);
            } else {
                self.draw_state(ui, &ctx);
            }
        });
    }
}

fn first_dropped_path(files: &[egui::DroppedFile]) -> Option<PathBuf> {
    files.iter().find_map(|file| file.path.clone())
}

fn start_send(
    path: PathBuf,
    cancel_send: &mut Option<oneshot::Sender<()>>,
    evt_tx: &std_mpsc::Sender<UiEvent>,
    ctx: &egui::Context,
) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    evt_tx.send(UiEvent::SendStarted { name }).ok();
    ctx.request_repaint();

    let (c_tx, c_rx) = oneshot::channel();
    *cancel_send = Some(c_tx);
    let (se_tx, mut se_rx) = tokio_mpsc::channel(64);
    tokio::spawn(send_file(path, se_tx, c_rx));

    let evt_tx = evt_tx.clone();
    let ctx = ctx.clone();
    tokio::spawn(async move {
        while let Some(e) = se_rx.recv().await {
            evt_tx.send(UiEvent::Send(e)).ok();
            ctx.request_repaint();
        }
    });
}

/// The background worker owns the tokio runtime and drives the actual
/// sendme send/receive operations.
fn spawn_worker(
    ctx: egui::Context,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<Command>,
    evt_tx: std_mpsc::Sender<UiEvent>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        rt.block_on(async move {
            let emit = |e: UiEvent| {
                evt_tx.send(e).ok();
                ctx.request_repaint();
            };
            let mut cancel_send: Option<oneshot::Sender<()>> = None;
            let mut recv_task: Option<tokio::task::JoinHandle<()>> = None;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::PickAndSend => {
                        let file = rfd::AsyncFileDialog::new()
                            .set_title("sendme: choose a file to send")
                            .pick_file()
                            .await;
                        match file {
                            None => emit(UiEvent::FilePickCancelled),
                            Some(file) => start_send(
                                file.path().to_path_buf(),
                                &mut cancel_send,
                                &evt_tx,
                                &ctx,
                            ),
                        }
                    }
                    Command::SendPath(path) => {
                        start_send(path, &mut cancel_send, &evt_tx, &ctx);
                    }
                    Command::CancelSend => {
                        if let Some(c) = cancel_send.take() {
                            c.send(()).ok();
                        }
                    }
                    Command::Receive { ticket } => match parse_ticket(&ticket) {
                        Err(e) => emit(UiEvent::TicketInvalid(e.to_string())),
                        Ok(ticket) => {
                            let dir = rfd::AsyncFileDialog::new()
                                .set_title("sendme: choose where to save")
                                .pick_folder()
                                .await;
                            match dir {
                                None => emit(UiEvent::FolderPickCancelled),
                                Some(dir) => {
                                    emit(UiEvent::ReceiveStarting);
                                    let (re_tx, mut re_rx) = tokio_mpsc::channel(64);
                                    recv_task = Some(tokio::spawn(receive_ticket(
                                        ticket,
                                        dir.path().to_path_buf(),
                                        re_tx,
                                    )));
                                    let evt_tx = evt_tx.clone();
                                    let ctx = ctx.clone();
                                    tokio::spawn(async move {
                                        while let Some(e) = re_rx.recv().await {
                                            evt_tx.send(UiEvent::Receive(e)).ok();
                                            ctx.request_repaint();
                                        }
                                    });
                                }
                            }
                        }
                    },
                    Command::CancelReceive => {
                        if let Some(task) = recv_task.take() {
                            task.abort();
                        }
                    }
                }
            }
        });
    });
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt::init();
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size(IDLE_SIZE)
            .with_decorations(false)
            .with_transparent(true)
            .with_drag_and_drop(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_app_id("sendme-balloon")
            .with_title("sendme balloon"),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    let options = {
        let mut options = options;
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var_os("DISPLAY").is_some()
        {
            // winit 0.30 has no Wayland data-device support. XWayland provides
            // working Xdnd events on compositors such as Sway/wlroots.
            options.event_loop_builder = Some(Box::new(|builder| {
                builder.with_x11();
            }));
        }
        options
    };
    eframe::run_native(
        "sendme balloon",
        options,
        Box::new(|cc| Ok(Box::new(BalloonApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_file_does_not_require_pointer_coordinates() {
        let path = PathBuf::from("example.txt");
        let files = vec![egui::DroppedFile {
            path: Some(path.clone()),
            ..Default::default()
        }];

        assert_eq!(first_dropped_path(&files), Some(path));
    }
}
