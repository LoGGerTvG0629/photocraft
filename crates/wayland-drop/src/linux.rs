//! The `wl_data_device` listener on winit's Wayland connection.
//!
//! winit 0.30 creates no data device, and egui's clipboard (smithay-clipboard) ignores drags, so
//! ours is the one that answers drag-and-drop offers for this client. It runs on its own event
//! queue in a worker thread; libwayland lets several threads read one connection (each
//! `prepare_read` / `read_events` pair dispatches to every queue), so winit's own dispatch is
//! untouched.
//!
//! A drop is read through a pipe (`wl_data_offer.receive`); the read end joins the worker's poll
//! set, so the worker never blocks on a slow source and the [`Listener`] can always stop it.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use raw_window_handle::{DisplayHandle, RawDisplayHandle, RawWindowHandle, WindowHandle};
use rustix::event::{PollFd, PollFlags, Timespec};
use wayland_client::backend::{Backend, ObjectId, WaylandError};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child};

use crate::{Dropped, parse_uri_list};

const URI_LIST: &str = "text/uri-list";
/// A source that hasn't finished writing its URI list by now is abandoned.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// More than any real list of paths; a larger body is cut off and parsed as far as it got.
const MAX_LIST_BYTES: usize = 4 << 20;

/// What the app reads each frame.
#[derive(Default)]
struct Shared {
    /// The pointer while an accepted drag is over our window.
    hover: Option<(f64, f64)>,
    /// The last drag position: the hover, or where the last drop landed.
    pointer: Option<(f64, f64)>,
    drops: Vec<Dropped>,
}

/// The running listener. Dropping it stops the worker and waits for it, so it must be dropped
/// before winit closes its display, as the app (which owns it) is.
pub struct Listener {
    shared: Arc<Mutex<Shared>>,
    /// Dropping the write end of this pipe wakes and stops the worker.
    stop: Option<OwnedFd>,
    worker: Option<JoinHandle<()>>,
}

impl Listener {
    /// Starts listening for file drops on the Wayland window of `display` and `window` (eframe's
    /// creation context has both). `wake` is called from the worker thread whenever something
    /// changed (a repaint request). `None` off Wayland, or when the compositor has no seat or
    /// data device manager.
    pub fn spawn(display: DisplayHandle<'_>, window: WindowHandle<'_>, wake: impl Fn() + Send + 'static) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = display.as_raw() else { return None };
        let RawWindowHandle::Wayland(window) = window.as_raw() else { return None };
        // SAFETY: `display` is winit's live `wl_display`. winit keeps it open for the life of its
        // event loop, and the backend made here (with every clone: the connection and the proxies
        // of our queue) lives only in this listener and its worker, which `Drop` joins. eframe
        // drops the app, which owns the listener, before its event loop. The backend is a guest:
        // it never closes winit's connection.
        #[allow(unsafe_code)]
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) = match registry_queue_init::<Worker>(&conn) {
            Ok(init) => init,
            Err(e) => {
                log::warn!("wayland drop: no registry: {e}");
                return None;
            }
        };
        let qh = queue.handle();
        let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=5, ()).map_err(|e| log::warn!("wayland drop: no seat: {e}")).ok()?;
        let manager = globals.bind::<WlDataDeviceManager, _, _>(&qh, 1..=3, ()).map_err(|e| log::warn!("wayland drop: no data device manager: {e}")).ok()?;
        let device = manager.get_data_device(&seat, &qh, ());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut worker = Worker {
            shared: shared.clone(),
            wake: Box::new(wake),
            surface: window.surface.as_ptr() as usize,
            mimes: HashMap::new(),
            drag: None,
            reading: None,
        };
        // Settle the data device's initial events (the current selection) before handing off.
        if let Err(e) = queue.roundtrip(&mut worker) {
            log::warn!("wayland drop: {e}");
            return None;
        }
        let (stop_rx, stop_tx) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
        let version = device.version();
        let thread = std::thread::Builder::new()
            .name("wayland-drop".into())
            .spawn(move || {
                if let Err(e) = run(&conn, &mut queue, &mut worker, &stop_rx) {
                    log::warn!("wayland drop: listener stopped: {e}");
                }
                worker.cleanup(&device, &seat, &manager);
                let _ = conn.flush();
            })
            .ok()?;
        log::info!("wayland drop: listening for file drops (data device v{version})");
        Some(Listener { shared, stop: Some(stop_tx), worker: Some(thread) })
    }

    /// Whether files are being dragged over the window now.
    pub fn hovering(&self) -> bool {
        lock(&self.shared).hover.is_some()
    }

    /// The pointer during the current or last drag, in surface-local logical pixels.
    pub fn pointer(&self) -> Option<(f64, f64)> {
        let shared = lock(&self.shared);
        shared.hover.or(shared.pointer)
    }

    /// The drops that arrived since the last call.
    pub fn take_drops(&self) -> Vec<Dropped> {
        std::mem::take(&mut lock(&self.shared).drops)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Dispatch until `stop` hangs up or the connection fails.
fn run(conn: &Connection, queue: &mut EventQueue<Worker>, worker: &mut Worker, stop: &OwnedFd) -> Result<(), WaylandError> {
    loop {
        queue.dispatch_pending(worker).map_err(|e| match e {
            wayland_client::DispatchError::Backend(e) => e,
            other => WaylandError::Io(std::io::Error::other(other.to_string())),
        })?;
        match conn.flush() {
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => other?,
        }
        // `None`: events are already queued for us; dispatch them first.
        let Some(guard) = queue.prepare_read() else { continue };
        let timeout = worker.reading.as_ref().map(|r| READ_TIMEOUT.saturating_sub(r.since.elapsed()));
        let timespec = timeout.map(|t| Timespec { tv_sec: t.as_secs() as _, tv_nsec: t.subsec_nanos() as _ });
        let (wayland, stopped, pipe) = {
            let mut fds = vec![PollFd::from_borrowed_fd(guard.connection_fd(), PollFlags::IN), PollFd::new(stop, PollFlags::IN)];
            if let Some(r) = &worker.reading {
                fds.push(PollFd::new(&r.fd, PollFlags::IN));
            }
            match rustix::event::poll(&mut fds, timespec.as_ref()) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(WaylandError::Io(e.into())),
            }
            let ready = |i: usize| fds.get(i).is_some_and(|f| !f.revents().is_empty());
            (ready(0), ready(1), ready(2))
        };
        if stopped {
            return Ok(());
        }
        if wayland {
            match guard.read() {
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                other => {
                    other?;
                }
            }
        } else {
            drop(guard);
        }
        if pipe {
            worker.pump_read();
        } else if worker.reading.as_ref().is_some_and(|r| r.since.elapsed() >= READ_TIMEOUT) {
            log::warn!("wayland drop: the drag source sent no file list in {READ_TIMEOUT:?}");
            if let Some(r) = worker.reading.take() {
                r.offer.destroy();
            }
        }
    }
}

/// A drag over one of our surfaces.
struct Drag {
    offer: Option<WlDataOffer>,
    /// It offers a URI list and is over our window.
    accepted: bool,
    at: (f64, f64),
}

/// A dropped URI list being read from the source.
struct Reading {
    fd: OwnedFd,
    buf: Vec<u8>,
    offer: WlDataOffer,
    at: (f64, f64),
    since: Instant,
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    wake: Box<dyn Fn() + Send>,
    /// Our window's `wl_surface` (its proxy address): drags over other surfaces (a client-side
    /// decoration, another window) are declined.
    surface: usize,
    /// The MIME types of each live offer, announced before the enter or selection that uses it.
    mimes: HashMap<ObjectId, Vec<String>>,
    drag: Option<Drag>,
    reading: Option<Reading>,
}

impl Worker {
    fn set_hover(&self, hover: Option<(f64, f64)>) {
        let mut shared = lock(&self.shared);
        shared.hover = hover;
        if hover.is_some() {
            shared.pointer = hover;
        }
        drop(shared);
        (self.wake)();
    }

    fn forget(&mut self, offer: &WlDataOffer) {
        self.mimes.remove(&offer.id());
        offer.destroy();
    }

    fn enter(&mut self, serial: u32, surface_ptr: usize, at: (f64, f64), offer: Option<WlDataOffer>) {
        self.leave();
        let ours = surface_ptr == self.surface;
        let accepted = ours && offer.as_ref().is_some_and(|o| self.mimes.get(&o.id()).is_some_and(|m| m.iter().any(|m| m == URI_LIST)));
        if let Some(o) = &offer {
            if accepted {
                o.accept(serial, Some(URI_LIST.into()));
                if o.version() >= 3 {
                    o.set_actions(DndAction::Copy, DndAction::Copy);
                }
            } else {
                o.accept(serial, None);
            }
        }
        self.drag = Some(Drag { offer, accepted, at });
        if accepted {
            self.set_hover(Some(at));
        }
    }

    fn motion(&mut self, at: (f64, f64)) {
        if let Some(drag) = self.drag.as_mut().filter(|d| d.accepted) {
            drag.at = at;
            self.set_hover(Some(at));
        }
    }

    fn leave(&mut self) {
        if let Some(drag) = self.drag.take() {
            if let Some(o) = &drag.offer {
                self.forget(o);
            }
            if drag.accepted {
                self.set_hover(None);
            }
        }
    }

    fn drop_files(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        let Some(offer) = drag.offer else { return };
        self.mimes.remove(&offer.id());
        if !drag.accepted {
            offer.destroy();
            return;
        }
        {
            let mut shared = lock(&self.shared);
            shared.hover = None;
            shared.pointer = Some(drag.at);
        }
        if let Some(previous) = self.reading.take() {
            previous.offer.destroy();
        }
        match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
            Ok((read, write)) => {
                offer.receive(URI_LIST.into(), write.as_fd());
                // Our copy of the write end closes now; the source's closes when it's done.
                drop(write);
                self.reading = Some(Reading { fd: read, buf: Vec::new(), offer, at: drag.at, since: Instant::now() });
            }
            Err(e) => {
                log::warn!("wayland drop: no pipe for the dropped list: {e}");
                offer.destroy();
            }
        }
        (self.wake)();
    }

    /// Read what the source wrote; at end of file, deliver the paths and finish the drag.
    fn pump_read(&mut self) {
        let Some(r) = self.reading.as_mut() else { return };
        let mut chunk = [0u8; 16 * 1024];
        let done = match rustix::io::read(&r.fd, &mut chunk) {
            Ok(0) => true,
            Ok(n) => {
                r.buf.extend_from_slice(&chunk[..n]);
                r.buf.len() >= MAX_LIST_BYTES
            }
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => false,
            Err(e) => {
                log::warn!("wayland drop: reading the dropped list: {e}");
                true
            }
        };
        if !done {
            return;
        }
        let Some(r) = self.reading.take() else { return };
        let paths = parse_uri_list(&String::from_utf8_lossy(&r.buf));
        if r.offer.version() >= 3 {
            r.offer.finish();
        }
        r.offer.destroy();
        if paths.is_empty() {
            log::info!("wayland drop: the dropped list had no local files");
            return;
        }
        lock(&self.shared).drops.push(Dropped { paths, at: r.at });
        (self.wake)();
    }

    fn cleanup(&mut self, device: &WlDataDevice, seat: &WlSeat, manager: &WlDataDeviceManager) {
        if let Some(o) = self.drag.take().and_then(|d| d.offer) {
            o.destroy();
        }
        if let Some(r) = self.reading.take() {
            r.offer.destroy();
        }
        if device.version() >= 2 {
            device.release();
        }
        if seat.version() >= 5 {
            seat.release();
        }
        let _ = manager;
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Worker {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(Worker: ignore WlSeat);
delegate_noop!(Worker: ignore WlDataDeviceManager);

impl Dispatch<WlDataDevice, ()> for Worker {
    fn event(worker: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                worker.mimes.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Enter { serial, surface, x, y, id } => {
                worker.enter(serial, surface.id().as_ptr() as usize, (x, y), id);
            }
            wl_data_device::Event::Motion { x, y, .. } => worker.motion((x, y)),
            wl_data_device::Event::Leave => worker.leave(),
            wl_data_device::Event::Drop => worker.drop_files(),
            // The clipboard is egui's (smithay-clipboard has its own data device): let go of ours.
            wl_data_device::Event::Selection { id: Some(o) } => worker.forget(&o),
            _ => {}
        }
    }

    event_created_child!(Worker, WlDataDevice, [wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ())]);
}

impl Dispatch<WlDataOffer, ()> for Worker {
    fn event(worker: &mut Self, offer: &WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            worker.mimes.entry(offer.id()).or_default().push(mime_type);
        }
    }
}
