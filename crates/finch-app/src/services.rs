use crate::{
    Message,
    platform::{Instance, InstanceMessage},
    plugin_panel,
};
use crossbeam_channel::{Receiver, Sender, bounded, select, select_biased};
use finch_pdf::{PdfEvent, PdfWorker};
use finch_plugin::{PluginContext, PluginHost, PluginInfo, PluginResult};
use finch_store::{AppPaths, StoreEvent, StoreWorker};
use futures::FutureExt;
use std::{
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Channel closure broadcasts cancellation to every blocked waiter.
struct StopSource {
    blocking: Option<Sender<()>>,
    asynchronous: async_channel::Sender<()>,
}

#[derive(Clone)]
struct StopToken {
    blocking: Receiver<()>,
    asynchronous: async_channel::Receiver<()>,
}

impl StopSource {
    fn new() -> (Self, StopToken) {
        let (blocking, receiver) = bounded(1);
        let (asynchronous, async_receiver) = async_channel::bounded(1);
        (
            Self {
                blocking: Some(blocking),
                asynchronous,
            },
            StopToken {
                blocking: receiver,
                asynchronous: async_receiver,
            },
        )
    }

    fn cancel(&mut self) {
        self.blocking.take();
        self.asynchronous.close();
    }
}

impl Drop for StopSource {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A full UI queue must not prevent workers from stopping. Park until delivery
/// or cancellation, without polling or closing the shared application bus.
fn publish<T: Send>(output: &async_channel::Sender<T>, stop: &StopToken, message: T) -> bool {
    futures::executor::block_on(async {
        futures::select_biased! {
            _ = stop.asynchronous.recv().fuse() => false,
            result = output.send(message).fuse() => result.is_ok(),
        }
    })
}

fn join(thread: &mut Option<JoinHandle<()>>, name: &str) -> Result<(), String> {
    thread.take().map_or(Ok(()), |thread| {
        thread.join().map_err(|_| format!("{name} thread panicked"))
    })
}

#[derive(Clone)]
pub struct EventStream(pub async_channel::Receiver<Message>);
impl Hash for EventStream {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "finch-events".hash(state);
    }
}
pub fn stream(source: &EventStream) -> impl futures::Stream<Item = Message> + use<> {
    // Iced's proxy has its own queue. Waiting for the UI acknowledgement
    // prevents that queue from silently buffering a hundred PDF bitmaps.
    futures::stream::unfold(
        (source.0.clone(), None::<async_channel::Receiver<()>>),
        |(source, previous)| async move {
            if let Some(previous) = previous {
                let _ = previous.recv().await;
            }
            let message = source.recv().await.ok()?;
            let (received, acknowledgement) = async_channel::bounded(1);
            Some((
                Message::Bridged {
                    message: Box::new(message),
                    received,
                },
                (source, Some(acknowledgement)),
            ))
        },
    )
}

/// Retain this guard in App. Dropping it cancels and joins the event bridge.
#[must_use = "retain the Bridge guard in App until shutdown"]
pub struct Bridge {
    stop: StopSource,
    thread: Option<JoinHandle<()>>,
}

pub fn bridge(
    pdf: &PdfWorker,
    store: &StoreWorker,
    instance: &Instance,
    output: async_channel::Sender<Message>,
) -> Bridge {
    bridge_receivers(pdf.events(), store.events(), instance.events(), output)
}

fn bridge_receivers(
    mut pdf: Receiver<PdfEvent>,
    mut store: Receiver<StoreEvent>,
    mut instance: Receiver<InstanceMessage>,
    output: async_channel::Sender<Message>,
) -> Bridge {
    let (stop, token) = StopSource::new();
    let thread = thread::Builder::new().name("finch-events".into()).spawn(move || {
        let mut active = 3;
        while active != 0 {
            let message = select! {
                recv(token.blocking) -> _ => break,
                recv(pdf) -> event => match event { Ok(event) => Message::Pdf(event), Err(_) => { pdf = crossbeam_channel::never(); active -= 1; continue; } },
                recv(store) -> event => match event { Ok(event) => Message::Store(event), Err(_) => { store = crossbeam_channel::never(); active -= 1; continue; } },
                recv(instance) -> event => match event { Ok(event) => Message::Instance(event), Err(_) => { instance = crossbeam_channel::never(); active -= 1; continue; } },
            };
            if !publish(&output, &token, message) { break; }
        }
    }).expect("create event bridge");
    Bridge {
        stop,
        thread: Some(thread),
    }
}

impl Bridge {
    #[cfg(test)]
    pub fn stop_for_test(&mut self) -> Result<(), String> {
        self.stop.cancel();
        join(&mut self.thread, "event bridge")
    }
    #[cfg(test)]
    pub fn shutdown(mut self) -> Result<(), String> {
        self.stop.cancel();
        join(&mut self.thread, "event bridge")
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.cancel();
        let _ = join(&mut self.thread, "event bridge");
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Timer {
    Save,
    Query,
    Quit,
    Capture,
    Smoke,
}
const TIMERS: [Timer; 5] = [
    Timer::Save,
    Timer::Query,
    Timer::Quit,
    Timer::Capture,
    Timer::Smoke,
];

impl Timer {
    fn index(self) -> usize {
        match self {
            Self::Save => 0,
            Self::Query => 1,
            Self::Quit => 2,
            Self::Capture => 3,
            Self::Smoke => 4,
        }
    }
}

/// One slot per timer kind and one wake token. Repeated scroll events replace
/// Save in place; a full wake queue never discards the final requested deadline.
pub struct Timers {
    deadlines: Arc<Mutex<[Option<Instant>; 5]>>,
    wake: Sender<()>,
    stop: StopSource,
    thread: Option<JoinHandle<()>>,
}

impl Timers {
    pub fn new(output: async_channel::Sender<Message>) -> Self {
        let deadlines = Arc::new(Mutex::new([None; 5]));
        let pending = Arc::clone(&deadlines);
        let (wake, receiver) = bounded(1);
        let (stop, token) = StopSource::new();
        let thread = thread::Builder::new().name("finch-timers".into()).spawn(move || {
            loop {
                let next = pending.lock().expect("timer deadlines lock").iter().flatten().copied().min();
                let timeout = next.map_or_else(crossbeam_channel::never, |next: Instant| crossbeam_channel::after(next.saturating_duration_since(Instant::now())));
                select_biased! {
                    recv(token.blocking) -> _ => break,
                    recv(timeout) -> _ => {
                        let now = Instant::now();
                        let mut due = [false; 5];
                        {
                            let mut deadlines = pending.lock().expect("timer deadlines lock");
                            for (index, deadline) in deadlines.iter_mut().enumerate() {
                                if deadline.is_some_and(|at| at <= now) { due[index] = true; *deadline = None; }
                            }
                        }
                        for (index, due) in due.into_iter().enumerate() {
                            if due && !publish(&output, &token, Message::Timer(TIMERS[index])) { return; }
                        }
                    },
                    recv(receiver) -> wake => if wake.is_err() { break; },
                }
            }
        }).expect("create timers");
        Self {
            deadlines,
            wake,
            stop,
            thread: Some(thread),
        }
    }

    pub fn after(&self, key: Timer, duration: Duration) {
        self.deadlines.lock().expect("timer deadlines lock")[key.index()] =
            Some(Instant::now() + duration);
        let _ = self.wake.try_send(()); // An existing token already covers this update.
    }

    #[cfg(test)]
    pub fn shutdown(mut self) -> Result<(), String> {
        self.stop.cancel();
        join(&mut self.thread, "timer")
    }
}
impl Drop for Timers {
    fn drop(&mut self) {
        self.stop.cancel();
        let _ = join(&mut self.thread, "timer");
    }
}

#[derive(Debug, Clone)]
pub enum PluginEvent {
    List(Result<Vec<PluginInfo>, String>),
    Result(Result<PluginResult, String>),
}
enum PluginCommand {
    Action(plugin_panel::Action, PluginContext),
    Install(Option<PathBuf>),
}

pub struct Plugins {
    sender: Option<Sender<PluginCommand>>,
    stop: StopSource,
    token: StopToken,
    thread: Option<JoinHandle<()>>,
    picking: Arc<AtomicBool>,
}

impl Plugins {
    pub fn spawn(paths: &AppPaths, output: async_channel::Sender<Message>) -> Self {
        let (sender, receiver) = bounded::<PluginCommand>(8);
        let (stop, token) = StopSource::new();
        let worker_token = token.clone();
        let dirs = (paths.plugins.clone(), paths.plugin_data.clone());
        let thread = thread::Builder::new().name("finch-plugins".into()).spawn(move || {
            let mut host = PluginHost::new(dirs.0, dirs.1);
            loop {
                let command = select_biased! {
                    recv(worker_token.blocking) -> _ => break,
                    recv(receiver) -> command => match command { Ok(command) => command, Err(_) => break },
                };
                let event = match command {
                    PluginCommand::Action(plugin_panel::Action::Run { plugin_id, command_id }, context) => PluginEvent::Result(host.invoke(&plugin_id, &command_id, context).map_err(|e| e.to_string())),
                    PluginCommand::Action(plugin_panel::Action::SetEnabled { id, enabled }, _) => PluginEvent::List(host.set_enabled(&id, enabled).and_then(|_| host.discover()).map_err(|e| e.to_string())),
                    PluginCommand::Install(picked) => {
                        let result = match picked { Some(path) => host.install(&path).and_then(|_| host.discover()), None => host.discover() };
                        PluginEvent::List(result.map_err(|e| e.to_string()))
                    }
                    PluginCommand::Action(_, _) => PluginEvent::List(host.discover().map_err(|e| e.to_string())),
                };
                if !publish(&output, &worker_token, Message::PluginEvent(event)) { break; }
            }
            host.shutdown();
        }).expect("create plugin supervisor");
        Self {
            sender: Some(sender),
            stop,
            token,
            thread: Some(thread),
            picking: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn send(&self, action: plugin_panel::Action, context: PluginContext) -> Result<(), String> {
        let sender = self
            .sender
            .as_ref()
            .ok_or("plugin supervisor has stopped")?;
        if !matches!(action, plugin_panel::Action::Install) {
            return sender
                .try_send(PluginCommand::Action(action, context))
                .map_err(|e| e.to_string());
        }
        if self.picking.swap(true, Ordering::AcqRel) {
            return Err("a plugin picker is already open".into());
        }
        let picking = Arc::clone(&self.picking);
        let sender = sender.clone();
        let stop = self.token.clone();
        // A user-controlled native dialog must not delay plugin process cleanup.
        if let Err(error) = thread::Builder::new()
            .name("finch-plugin-picker".into())
            .spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_title("选择插件清单 plugin.json")
                    .add_filter("Finch 插件", &["json"])
                    .pick_file();
                select_biased! {
                    recv(stop.blocking) -> _ => {},
                    send(sender, PluginCommand::Install(picked)) -> _ => {},
                }
                picking.store(false, Ordering::Release);
            })
        {
            self.picking.store(false, Ordering::Release);
            return Err(error.to_string());
        }
        Ok(())
    }

    /// Join after any in-flight invocation reaches its startup/command timeout
    /// and all owned child processes are reaped. Queued commands are discarded on
    /// exit, not executed after the user has closed the application.
    pub fn shutdown(mut self) -> Result<(), String> {
        self.stop.cancel();
        self.sender.take();
        join(&mut self.thread, "plugin supervisor")
    }
}
impl Drop for Plugins {
    fn drop(&mut self) {
        // Explicit shutdown joins outside the UI callback. This fallback still
        // wakes the worker and releases any pending UI event publication.
        self.stop.cancel();
        self.sender.take();
    }
}

pub fn dialog(output: async_channel::Sender<Message>, relink: Option<String>) {
    thread::Builder::new()
        .name("finch-file-dialog".into())
        .spawn(move || {
            let dialog = rfd::FileDialog::new()
                .set_title("打开论文")
                .add_filter("PDF", &["pdf"]);
            let paths = if relink.is_some() {
                dialog.pick_file().into_iter().collect()
            } else {
                dialog.pick_files().unwrap_or_default()
            };
            let _ = output.send_blocking(Message::FilesPicked { paths, relink });
        })
        .expect("create file dialog");
}

pub struct SmallExecutor(futures::executor::ThreadPool);
impl iced::Executor for SmallExecutor {
    fn new() -> std::io::Result<Self> {
        Ok(Self(
            futures::executor::ThreadPoolBuilder::new()
                .pool_size(1)
                .name_prefix("finch-async-")
                .create()?,
        ))
    }
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.0.spawn_ok(future);
    }
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        futures::executor::block_on(future)
    }
}

/// Save lossless P6 PPM. Replace the requested extension with `.ppm`, so
/// `--screenshot view.png` writes `view.ppm`. No image codec is linked for this.
pub fn write_screenshot(
    mut path: PathBuf,
    screenshot: iced::window::Screenshot,
) -> Result<(), String> {
    use std::io::Write;
    let expected = (screenshot.size.width as usize)
        .checked_mul(screenshot.size.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("screenshot dimensions overflow")?;
    if screenshot.size.width == 0
        || screenshot.size.height == 0
        || screenshot.rgba.len() != expected
    {
        return Err("screenshot dimensions do not match RGBA data".into());
    }
    path.set_extension("ppm");
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    write!(
        file,
        "P6\n{} {}\n255\n",
        screenshot.size.width, screenshot.size.height
    )
    .map_err(|e| e.to_string())?;
    for pixel in screenshot.rgba.as_chunks::<4>().0 {
        file.write_all(&pixel[..3]).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_stream_waits_for_ui_acknowledgement_before_dequeuing_next_bitmap() {
        use futures::StreamExt;
        let (sender, receiver) = async_channel::bounded(1);
        let source = EventStream(receiver.clone());
        let mut events = Box::pin(stream(&source));
        sender.try_send(Message::DismissError).unwrap();
        let first = futures::executor::block_on(events.next()).unwrap();
        sender.try_send(Message::DismissError).unwrap();
        assert!(events.next().now_or_never().is_none());
        assert_eq!(
            receiver.len(),
            1,
            "the framework may not prefetch the second event"
        );
        let Message::Bridged { received, .. } = first else {
            panic!("missing acknowledgement")
        };
        received.try_send(()).unwrap();
        assert!(futures::executor::block_on(events.next()).is_some());
        assert!(receiver.is_empty());
    }

    #[test]
    fn cancellation_releases_a_worker_blocked_by_a_full_ui_queue() {
        let (sender, _receiver) = async_channel::bounded(1);
        sender.send_blocking(1).unwrap();
        let (mut source, token) = StopSource::new();
        let (done, finished) = bounded(1);
        let thread = thread::spawn(move || {
            done.send(publish(&sender, &token, 2)).unwrap();
        });
        source.cancel();
        assert!(!finished.recv_timeout(Duration::from_secs(1)).unwrap());
        thread.join().unwrap();
    }

    #[test]
    fn bridge_exits_when_all_inputs_close_and_when_output_is_full() {
        let (pdf_tx, pdf_rx) = bounded(1);
        let (store_tx, store_rx) = bounded(1);
        let (instance_tx, instance_rx) = bounded(1);
        let (output, events) = async_channel::bounded(1);
        let bridge = bridge_receivers(pdf_rx, store_rx, instance_rx, output);
        drop((pdf_tx, store_tx, instance_tx));
        let until = Instant::now() + Duration::from_secs(1);
        while !events.is_closed() && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            events.is_closed(),
            "bridge must release output when all sources stop"
        );
        bridge.shutdown().unwrap();

        let (_pdf_tx, pdf_rx) = bounded(1);
        let (_store_tx, store_rx) = bounded(1);
        let (instance_tx, instance_rx) = bounded(1);
        let (output, _events) = async_channel::bounded(1);
        output.send_blocking(Message::Timer(Timer::Save)).unwrap();
        let bridge = bridge_receivers(pdf_rx, store_rx, instance_rx, output);
        instance_tx.send(InstanceMessage::Focus).unwrap();
        bridge.shutdown().unwrap();
    }

    #[test]
    fn timers_coalesce_bursts_without_losing_the_last_deadline() {
        let (output, events) = async_channel::bounded(2);
        let timers = Timers::new(output);
        for _ in 0..10_000 {
            timers.after(Timer::Save, Duration::from_secs(30));
        }
        timers.after(Timer::Save, Duration::from_millis(5));
        let until = Instant::now() + Duration::from_secs(1);
        let event = loop {
            if let Ok(message) = events.try_recv() {
                break Some(message);
            }
            if Instant::now() >= until {
                break None;
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(event, Some(Message::Timer(Timer::Save))));
        assert!(events.try_recv().is_err());
        timers.shutdown().unwrap();
    }

    #[test]
    fn plugin_supervisor_joins_without_ui_consuming_its_response() {
        let temp = tempfile::TempDir::new().unwrap();
        let (output, _events) = async_channel::bounded(1);
        output.send_blocking(Message::Timer(Timer::Save)).unwrap();
        let plugins = Plugins::spawn(&AppPaths::in_directory(temp.path()), output);
        plugins
            .send(plugin_panel::Action::Discover, PluginContext::default())
            .unwrap();
        plugins.shutdown().unwrap();
    }

    #[test]
    fn screenshots_document_ppm_extension_and_validate_pixel_count() {
        let temp = tempfile::TempDir::new().unwrap();
        let requested = temp.path().join("view.png");
        let screenshot = iced::window::Screenshot::new(
            vec![1, 2, 3, 255, 4, 5, 6, 255],
            iced::Size::new(2, 1),
            1.0,
        );
        write_screenshot(requested.clone(), screenshot).unwrap();
        assert!(!requested.exists());
        assert_eq!(
            std::fs::read(requested.with_extension("ppm")).unwrap(),
            b"P6\n2 1\n255\n\x01\x02\x03\x04\x05\x06"
        );
        let invalid = iced::window::Screenshot::new(vec![0; 3], iced::Size::new(1, 1), 1.0);
        assert!(write_screenshot(temp.path().join("invalid"), invalid).is_err());
    }
}
