//! Linux clipboard via `org.freedesktop.portal.Clipboard` (Plasma 6.4+).
//!
//! The portal does not create its own session: it extends the RemoteDesktop
//! session owned by `oink-input`. [`PortalClipboardSlot`] is filled after
//! `RequestClipboard` succeeds, and cleared when that session drops.
//!
//! Text only for now — images and files are not published. Transfers use the
//! MIME ownership protocol (`SetSelection` / `SelectionTransfer` /
//! `SelectionRead`). Own writes are filtered with `session_is_owner`.

use std::io::{self, Read, Write};
use std::os::fd::OwnedFd as StdOwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::clipboard::{Clipboard, SelectionOwnerChanged, SetSelectionOptions};
use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::desktop::Session;
use futures_util::StreamExt;
use oink_foundation::DeviceId;
use oink_foundation::clipboard::{ClipData, ClipPlatform, ClipSnapshot, Sensitivity};
use oink_foundation::content::{
    ClipId, ContentItem, ContentSize, ItemId, Medium, NativeFormat, OfferId, Representation, RepresentationData,
    RepresentationId, StandardFormat, Unavailable,
};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::Notify;

use crate::content::{self, Provider, Translator};
use crate::{ChangeHandler, ClipboardBackend, ClipboardError, NativeSpec};

const TEXT_UTF8: &str = "text/plain;charset=utf-8";
const TEXT_PLAIN: &str = "text/plain";
const TEXT_MIMES: &[&str] = &[TEXT_UTF8, TEXT_PLAIN];

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("oink-clipboard")
            .build()
            .expect("clipboard runtime")
    })
}

fn block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(|| runtime().block_on(fut))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    } else {
        runtime().block_on(fut)
    }
}

/// Shared bind point between `oink-input` (RemoteDesktop lifetime) and this backend.
pub struct PortalClipboardSlot {
    bound: Mutex<Option<Arc<Bound>>>,
    /// Wakes watch/transfer loops when the RemoteDesktop session binds or unbinds.
    changed: Notify,
}

impl std::fmt::Debug for PortalClipboardSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bound = self.bound.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        f.debug_struct("PortalClipboardSlot").field("bound", &bound).finish()
    }
}

struct Bound {
    clipboard: Clipboard,
    session: Arc<Session<RemoteDesktop>>,
    /// D-Bus object-path encoding of `session` for signal filtering.
    session_key: Vec<u8>,
}

impl PortalClipboardSlot {
    pub fn new() -> Self {
        Self {
            bound: Mutex::new(None),
            changed: Notify::new(),
        }
    }

    /// Attach clipboard access for a live RemoteDesktop session.
    pub fn bind(&self, clipboard: Clipboard, session: Arc<Session<RemoteDesktop>>) {
        let session_key = session_key(session.as_ref());
        *self.bound.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Arc::new(Bound { clipboard, session, session_key }));
        self.changed.notify_waiters();
    }

    pub fn unbind(&self) {
        *self.bound.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.changed.notify_waiters();
    }

    fn get(&self) -> Result<Arc<Bound>, ClipboardError> {
        self.bound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                ClipboardError::Platform(
                    "剪贴板 portal 尚未绑定（需要运行中的 Oink RemoteDesktop 会话）".into(),
                )
            })
    }
}

/// Portal-backed Linux clipboard. Operations fail until [`PortalClipboardSlot::bind`].
pub fn portal_backend(slot: Arc<PortalClipboardSlot>) -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if !session.eq_ignore_ascii_case("wayland") && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(ClipboardError::Platform(
            "需要 Wayland 会话（XDG_SESSION_TYPE=wayland 或 WAYLAND_DISPLAY）".into(),
        ));
    }
    let offer = Arc::new(tokio::sync::Mutex::new(None));
    let transfer = TransferServer::start(Arc::clone(&slot), Arc::clone(&offer))?;
    Ok(Box::new(LinuxClipboard {
        slot,
        last: Arc::new(tokio::sync::Mutex::new(String::new())),
        offer,
        change_count: Arc::new(AtomicU64::new(1)),
        watcher: Mutex::new(None),
        transfer: Mutex::new(Some(transfer)),
        watch_paused: Arc::new(AtomicBool::new(false)),
    }))
}

/// Same as [`portal_backend`] with an unbound slot (standalone helpers / tests).
pub(crate) fn backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    portal_backend(Arc::new(PortalClipboardSlot::new()))
}

struct LinuxClipboard {
    slot: Arc<PortalClipboardSlot>,
    last: Arc<tokio::sync::Mutex<String>>,
    /// Text we currently advertise via `SetSelection`, served on `SelectionTransfer`.
    offer: Arc<tokio::sync::Mutex<Option<String>>>,
    change_count: Arc<AtomicU64>,
    watcher: Mutex<Option<Watcher>>,
    /// Serves `SelectionTransfer` for our `SetSelection` even when watch is off.
    transfer: Mutex<Option<TransferServer>>,
    watch_paused: Arc<AtomicBool>,
}

impl LinuxClipboard {
    async fn read_text_async(&self) -> Result<String, ClipboardError> {
        let bound = self.slot.get()?;
        read_plain_text(&bound.clipboard, bound.session.as_ref()).await
    }

    async fn write_text_async(&self, text: &str) -> Result<u64, ClipboardError> {
        let bound = self.slot.get()?;
        {
            let mut offer = self.offer.lock().await;
            *offer = Some(text.to_owned());
        }
        bound
            .clipboard
            .set_selection(
                bound.session.as_ref(),
                SetSelectionOptions::default().set_mime_types(TEXT_MIMES),
            )
            .await
            .map_err(|e| ClipboardError::Platform(format!("SetSelection：{e}")))?;
        let mut last = self.last.lock().await;
        text.clone_into(&mut *last);
        Ok(self.change_count.fetch_add(1, Ordering::AcqRel) + 1)
    }
}

fn session_key(session: &Session<RemoteDesktop>) -> Vec<u8> {
    use zbus::wire::{Endian, serialized::Context, to_bytes};
    let ctx = Context::new(Endian::Little, 0);
    to_bytes(ctx, session)
        .map(|data| data.bytes().to_vec())
        .unwrap_or_default()
}

fn same_session(bound: &Bound, event: &Session<RemoteDesktop>) -> bool {
    !bound.session_key.is_empty() && bound.session_key == session_key(event)
}

/// Successful empty read is `Ok("")`. Every MIME failing is `Err` (do not treat as clear).
fn text_from_selection_reads(results: &[Result<Vec<u8>, String>]) -> Result<String, ClipboardError> {
    let mut last_err = None;
    for result in results {
        match result {
            Ok(bytes) => return decode_text(bytes),
            Err(error) => last_err = Some(error.clone()),
        }
    }
    Err(ClipboardError::Platform(format!(
        "SelectionRead：{}",
        last_err.unwrap_or_else(|| "no text MIME".into())
    )))
}

async fn read_plain_text(clipboard: &Clipboard, session: &Session<RemoteDesktop>) -> Result<String, ClipboardError> {
    let mut results = Vec::with_capacity(TEXT_MIMES.len());
    for mime in TEXT_MIMES {
        match clipboard.selection_read(session, mime).await {
            Ok(fd) => match read_all_fd(fd) {
                Ok(bytes) => results.push(Ok(bytes)),
                Err(error) => results.push(Err(error.to_string())),
            },
            Err(error) => {
                tracing::trace!(%error, mime, "SelectionRead mime unavailable");
                results.push(Err(error.to_string()));
            }
        }
    }
    text_from_selection_reads(&results)
}

fn read_all_fd(fd: zbus::wire::OwnedFd) -> io::Result<Vec<u8>> {
    let std_fd: StdOwnedFd = fd.into();
    let mut file = std::fs::File::from(std_fd);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn write_all_fd(fd: zbus::wire::OwnedFd, bytes: &[u8]) -> io::Result<()> {
    let std_fd: StdOwnedFd = fd.into();
    let mut file = std::fs::File::from(std_fd);
    file.write_all(bytes)?;
    Ok(())
}

fn decode_text(bytes: &[u8]) -> Result<String, ClipboardError> {
    let text = String::from_utf8_lossy(bytes);
    Ok(text.trim_end_matches('\0').to_owned())
}

impl ClipboardBackend for LinuxClipboard {
    fn change_count(&self) -> u64 {
        self.change_count.load(Ordering::Acquire)
    }

    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        let text = block_on(self.read_text_async())?;
        let mut items = Vec::new();
        if !text.is_empty() {
            items.push(ClipData::Text(text));
        }
        Ok(ClipSnapshot {
            items,
            sensitivity: Sensitivity::Normal,
            change_count: self.change_count(),
        })
    }

    fn capture(&self, _data: &std::path::Path) -> Result<crate::CapturedClipboard, ClipboardError> {
        let count = self.change_count();
        let text = block_on(self.read_text_async())?;
        if self.change_count() != count {
            return Err(ClipboardError::Busy);
        }
        let slot = Arc::clone(&self.slot);
        let counts = Arc::clone(&self.change_count);
        let provider = serve_text(count, text, move || {
            if counts.load(Ordering::Acquire) != count {
                return Err(content::clipboard::changed());
            }
            let bound = slot.get().map_err(|e| io::Error::other(e.to_string()))?;
            let current = block_on(read_plain_text(&bound.clipboard, bound.session.as_ref()))
                .map_err(|e| io::Error::other(e.to_string()))?;
            if counts.load(Ordering::Acquire) != count {
                return Err(content::clipboard::changed());
            }
            Ok(current)
        })
        .map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?;
        if self.change_count() != count {
            return Err(ClipboardError::Busy);
        }
        Ok(crate::CapturedClipboard {
            snapshot: ClipSnapshot {
                items: content::items(&provider.inventory()),
                sensitivity: Sensitivity::Normal,
                change_count: count,
            },
            content: Some(provider),
        })
    }

    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        publishable_sensitivity(sensitivity)?;
        let text = items.iter().find_map(|item| match item {
            ClipData::Text(s) => Some(s.as_str()),
            _ => None,
        });
        let Some(text) = text else {
            return Err(ClipboardError::Unsupported);
        };
        block_on(self.write_text_async(text))
    }

    fn write_native(&self, spec: NativeSpec) -> Result<u64, ClipboardError> {
        self.write_native_guarded(spec, Arc::new(|| Ok(())))
    }

    fn write_native_guarded(&self, spec: NativeSpec, guard: content::ReaderGuard) -> Result<u64, ClipboardError> {
        self.write_native_cancellable(spec, guard, Arc::new(|| Ok(())))
    }

    fn write_native_cancellable(
        &self,
        spec: NativeSpec,
        guard: content::ReaderGuard,
        current: content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        // File publication is not implemented. The caller retries the text
        // companions after this error instead of leaving them unpublished.
        if !spec.files.is_empty() {
            return Err(ClipboardError::FileProviderUnavailable);
        }
        current().map_err(|_| ClipboardError::Busy)?;
        let text = plain_text(&spec)?;
        guard().map_err(|_| ClipboardError::Busy)?;
        self.write(&[ClipData::Text(text)], spec.sensitivity)
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), ClipboardError> {
        let mut slot = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(existing) => {
                existing.add(on_change);
                Ok(())
            }
            None => {
                *slot = Some(Watcher::start(
                    Arc::clone(&self.slot),
                    Arc::clone(&self.last),
                    Arc::clone(&self.change_count),
                    Arc::clone(&self.watch_paused),
                    on_change,
                )?);
                Ok(())
            }
        }
    }

    fn set_watch_paused(&self, paused: bool) -> Result<(), ClipboardError> {
        self.watch_paused.store(paused, Ordering::Release);
        Ok(())
    }
}

struct Watcher {
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    handlers: Arc<Mutex<Vec<ChangeHandler>>>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    fn start(
        slot: Arc<PortalClipboardSlot>,
        last: Arc<tokio::sync::Mutex<String>>,
        change_count: Arc<AtomicU64>,
        paused: Arc<AtomicBool>,
        first: ChangeHandler,
    ) -> Result<Self, ClipboardError> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let handlers = Arc::new(Mutex::new(vec![first]));
        let thread_stop = Arc::clone(&stop);
        let thread_wake = Arc::clone(&wake);
        let thread_handlers = Arc::clone(&handlers);
        let thread = std::thread::Builder::new()
            .name("oink-clipboard-watch".into())
            .spawn(move || {
                runtime().block_on(run_watch(
                    slot,
                    last,
                    thread_stop,
                    thread_wake,
                    paused,
                    thread_handlers,
                    change_count,
                ));
            })
            .map_err(|e| ClipboardError::Platform(format!("启动剪贴板监听失败：{e}")))?;
        Ok(Self {
            stop,
            wake,
            handlers,
            thread: Some(thread),
        })
    }

    fn add(&self, on_change: ChangeHandler) {
        self.handlers.lock().unwrap_or_else(|e| e.into_inner()).push(on_change);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct TransferServer {
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    thread: Option<JoinHandle<()>>,
}

impl TransferServer {
    fn start(
        slot: Arc<PortalClipboardSlot>,
        offer: Arc<tokio::sync::Mutex<Option<String>>>,
    ) -> Result<Self, ClipboardError> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let thread_stop = Arc::clone(&stop);
        let thread_wake = Arc::clone(&wake);
        let thread = std::thread::Builder::new()
            .name("oink-clipboard-xfer".into())
            .spawn(move || {
                runtime().block_on(run_transfer(slot, offer, thread_stop, thread_wake));
            })
            .map_err(|e| ClipboardError::Platform(format!("启动剪贴板 transfer 失败：{e}")))?;
        Ok(Self {
            stop,
            wake,
            thread: Some(thread),
        })
    }
}

impl Drop for TransferServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run_watch(
    slot: Arc<PortalClipboardSlot>,
    last: Arc<tokio::sync::Mutex<String>>,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    paused: Arc<AtomicBool>,
    handlers: Arc<Mutex<Vec<ChangeHandler>>>,
    change_count: Arc<AtomicU64>,
) {
    while !stop.load(Ordering::Acquire) {
        let Some(bound) = slot.bound.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            tokio::select! {
                _ = wake.notified() => return,
                _ = slot.changed.notified() => continue,
            }
        };

        let owner_stream = match bound.clipboard.receive_selection_owner_changed::<RemoteDesktop>().await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "订阅 SelectionOwnerChanged 失败");
                tokio::select! {
                    _ = wake.notified() => return,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                }
            }
        };

        let mut owner_stream = std::pin::pin!(owner_stream);
        loop {
            tokio::select! {
                _ = wake.notified() => return,
                _ = slot.changed.notified() => break,
                event = owner_stream.next() => {
                    let Some((event_session, change)) = event else { break };
                    if !same_session(&bound, &event_session) {
                        continue;
                    }
                    if paused.load(Ordering::Acquire) {
                        continue;
                    }
                    if !observe_change(&bound.clipboard, bound.session.as_ref(), &change, &last, &change_count).await {
                        continue;
                    }
                    let list = handlers.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    for handler in list {
                        handler();
                    }
                }
            }
        }
    }
}

async fn run_transfer(
    slot: Arc<PortalClipboardSlot>,
    offer: Arc<tokio::sync::Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    while !stop.load(Ordering::Acquire) {
        let Some(bound) = slot.bound.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            tokio::select! {
                _ = wake.notified() => return,
                _ = slot.changed.notified() => continue,
            }
        };

        let transfer_stream = match bound.clipboard.receive_selection_transfer::<RemoteDesktop>().await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "订阅 SelectionTransfer 失败");
                tokio::select! {
                    _ = wake.notified() => return,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                }
            }
        };

        let mut transfer_stream = std::pin::pin!(transfer_stream);
        loop {
            tokio::select! {
                _ = wake.notified() => return,
                _ = slot.changed.notified() => break,
                event = transfer_stream.next() => {
                    let Some((event_session, mime, serial)) = event else { break };
                    if !same_session(&bound, &event_session) {
                        continue;
                    }
                    serve_transfer(&bound.clipboard, bound.session.as_ref(), &mime, serial, &offer).await;
                }
            }
        }
    }
}

async fn observe_change(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    change: &SelectionOwnerChanged,
    last: &tokio::sync::Mutex<String>,
    change_count: &AtomicU64,
) -> bool {
    if change.session_is_owner() == Some(true) {
        return false;
    }
    let mimes = change.mime_types();
    if mimes.is_empty() {
        let mut last = last.lock().await;
        if last.is_empty() {
            return false;
        }
        last.clear();
        change_count.fetch_add(1, Ordering::AcqRel);
        return true;
    }
    if !mimes.iter().any(|mime| mime.starts_with("text/plain")) {
        return false;
    }
    let Ok(text) = read_plain_text(clipboard, session).await else {
        // Read failure must not look like an empty clipboard.
        return false;
    };
    let mut last = last.lock().await;
    if text == *last {
        return false;
    }
    *last = text;
    change_count.fetch_add(1, Ordering::AcqRel);
    true
}

async fn serve_transfer(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    mime: &str,
    serial: u32,
    offer: &tokio::sync::Mutex<Option<String>>,
) {
    let text = offer.lock().await.clone();
    let Some(text) = text else {
        let _ = clipboard.selection_write_done(session, serial, false).await;
        return;
    };
    if mime != TEXT_UTF8 && mime != TEXT_PLAIN {
        let _ = clipboard.selection_write_done(session, serial, false).await;
        return;
    }
    let Ok(fd) = clipboard.selection_write(session, serial).await else {
        let _ = clipboard.selection_write_done(session, serial, false).await;
        return;
    };
    let ok = write_all_fd(fd, text.as_bytes()).is_ok();
    let _ = clipboard.selection_write_done(session, serial, ok).await;
}

struct Utf8Text;
impl Translator for Utf8Text {
    fn standard(&self, representation: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        if representation.standard != Some(StandardFormat::Text) {
            return Err(content::unavailable(Unavailable::Unsupported));
        }
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| content::unavailable(Unavailable::InvalidData))?;
        Ok(vec![ClipData::Text(text)])
    }
}

fn text_inventory(count: u64, text: &str) -> crate::model::Inventory {
    let mut inventory = crate::model::Inventory {
        id: OfferId {
            clip: ClipId {
                origin: DeviceId([0; 32]),
                seq: 0,
            },
            generation: count,
        },
        platform: ClipPlatform::Unknown,
        items: Vec::new(),
        roots: Vec::new(),
    };
    if text.is_empty() {
        return inventory;
    }
    inventory.items.push(ContentItem {
        id: ItemId(1),
        virtual_files: false,
        file: None,
        representations: vec![Representation {
            id: RepresentationId(1),
            format: NativeFormat::Unknown("mime:text/plain;charset=utf-8".into()),
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            standard: Some(StandardFormat::Text),
            data: RepresentationData::Reader {
                size: ContentSize::Known(text.len() as u64),
            },
        }],
    });
    inventory
}

fn serve_text(
    count: u64,
    text: String,
    read: impl Fn() -> io::Result<String> + Send + Sync + 'static,
) -> io::Result<Arc<dyn Provider>> {
    let inventory = text_inventory(count, &text);
    let expected = text;
    let provider = content::clipboard::Clipboard::new(
        inventory,
        Box::new(move |id| {
            if id != RepresentationId(1) {
                return Err(content::unavailable(Unavailable::NoSuchItem));
            }
            let current = read()?;
            if current != expected {
                return Err(content::clipboard::changed());
            }
            Ok(Box::new(std::io::Cursor::new(current.into_bytes())))
        }),
        Arc::new(|| Ok(())),
        Arc::new(Utf8Text),
        None,
    )?;
    Ok(provider)
}

/// Portal SetSelection cannot mark Concealed/Transient/AutoGenerated.
/// Writing them as plain text would land secrets in history — fail closed.
fn publishable_sensitivity(sensitivity: Sensitivity) -> Result<(), ClipboardError> {
    match sensitivity {
        Sensitivity::Normal => Ok(()),
        Sensitivity::Concealed | Sensitivity::Transient | Sensitivity::AutoGenerated => {
            Err(ClipboardError::Unsupported)
        }
    }
}

fn plain_text(spec: &NativeSpec) -> Result<String, ClipboardError> {
    let target = crate::reconstruction_target();
    let mut saw_text = false;
    for representation in spec.inventory.wanted(target) {
        if representation.standard != Some(StandardFormat::Text) {
            continue;
        }
        if matches!(
            representation.data,
            RepresentationData::Unavailable(_) | RepresentationData::File(_) | RepresentationData::Promise(..)
        ) {
            continue;
        }
        saw_text = true;
        // Same-platform Windows text is still CF_UNICODETEXT. Every other
        // source was requested as the portable UTF-8 contract.
        let native_unicode =
            spec.inventory.platform == target && representation.format == NativeFormat::WindowsPredefined(13);
        if spec.inventory.platform == target && !native_unicode {
            continue;
        }
        let mut reader = match spec.contents.open(representation.id) {
            Ok(reader) => reader,
            Err(error) => return Err(read_error(error)),
        };
        let bytes = match content::read_all(&mut reader) {
            Ok(bytes) => bytes,
            Err(error) => return Err(read_error(error)),
        };
        let Some(text) = (if native_unicode {
            decode_windows_unicode(&bytes)
        } else {
            String::from_utf8(bytes).ok()
        }) else {
            continue;
        };
        return Ok(text);
    }
    Err(if saw_text {
        ClipboardError::ContentUnavailable(Unavailable::InvalidData)
    } else {
        ClipboardError::Unsupported
    })
}

fn read_error(error: io::Error) -> ClipboardError {
    let reason = content::reason(&error);
    if matches!(reason, Unavailable::Cancelled | Unavailable::Superseded) {
        ClipboardError::Busy
    } else {
        ClipboardError::ContentUnavailable(reason)
    }
}

/// `CF_UNICODETEXT`: UTF-16LE, optional trailing NUL, Windows newlines folded.
fn decode_windows_unicode(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|word| u16::from_le_bytes([word[0], word[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    let text = String::from_utf16(&words).ok()?;
    Some(text.replace("\r\n", "\n").replace('\r', "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_offer_is_portable_utf8_for_mac_and_windows() {
        let provider = serve_text(3, "你好".into(), || Ok("你好".into())).unwrap();
        let inventory = provider.inventory();
        assert!(inventory.validate().is_ok());
        for target in [ClipPlatform::Mac, ClipPlatform::Windows] {
            let representation = inventory.wanted(target).next().expect("text");
            assert_eq!(representation.standard, Some(StandardFormat::Text));
            let mut reader = content::open_for(provider.as_ref(), representation.id, target).unwrap();
            assert_eq!(content::read_all(&mut reader).unwrap(), "你好".as_bytes());
        }
    }

    #[test]
    fn empty_clipboard_has_no_items() {
        let provider = serve_text(1, String::new(), || Ok(String::new())).unwrap();
        assert!(provider.inventory().items.is_empty());
        assert!(provider.inventory().validate().is_ok());
    }

    #[test]
    fn windows_unicode_text_becomes_linux_newlines() {
        let bytes: Vec<u8> = "a\r\nb\0".encode_utf16().flat_map(|unit| unit.to_le_bytes()).collect();
        assert_eq!(decode_windows_unicode(&bytes).as_deref(), Some("a\nb"));
        assert!(decode_windows_unicode(&[0]).is_none());
    }

    #[test]
    fn non_normal_sensitivity_is_not_publishable() {
        assert!(publishable_sensitivity(Sensitivity::Normal).is_ok());
        for sensitivity in [
            Sensitivity::Concealed,
            Sensitivity::Transient,
            Sensitivity::AutoGenerated,
        ] {
            assert!(matches!(
                publishable_sensitivity(sensitivity),
                Err(ClipboardError::Unsupported)
            ));
        }
    }

    struct BytesOffer {
        inventory: crate::model::Inventory,
        bytes: Vec<u8>,
    }

    impl Provider for BytesOffer {
        fn inventory(&self) -> crate::model::Inventory {
            self.inventory.clone()
        }

        fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
            if id == RepresentationId(1) {
                Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
            } else {
                Err(content::unavailable(Unavailable::Unsupported))
            }
        }

        fn translator(&self) -> Arc<dyn Translator> {
            Arc::new(Utf8Text)
        }
    }

    fn text_spec(platform: ClipPlatform, format: NativeFormat, medium: Medium, bytes: Vec<u8>) -> NativeSpec {
        let inventory = crate::model::Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([0; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform,
            items: vec![ContentItem {
                id: ItemId(1),
                file: None,
                virtual_files: false,
                representations: vec![Representation {
                    id: RepresentationId(1),
                    format,
                    medium,
                    aspect: 1,
                    index: -1,
                    standard: Some(StandardFormat::Text),
                    data: RepresentationData::Reader {
                        size: ContentSize::Known(bytes.len() as u64),
                    },
                }],
            }],
            roots: Vec::new(),
        };
        NativeSpec {
            inventory: inventory.clone(),
            contents: Arc::new(BytesOffer { inventory, bytes }),
            files: Vec::new(),
            file_contents: None,
            sensitivity: Sensitivity::Normal,
        }
    }

    #[test]
    fn plain_text_reconstructs_windows_unicode_and_portable_utf8() {
        let utf16: Vec<u8> = "hi\r\n"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .chain([0, 0])
            .collect();
        let windows = text_spec(
            ClipPlatform::Windows,
            NativeFormat::WindowsPredefined(13),
            Medium::HGlobal,
            utf16,
        );
        assert_eq!(plain_text(&windows).unwrap(), "hi\n");

        let mac = text_spec(
            ClipPlatform::Mac,
            NativeFormat::Mac("public.utf8-plain-text".into()),
            Medium::Bytes,
            b"mac".to_vec(),
        );
        assert_eq!(plain_text(&mac).unwrap(), "mac");

        let unknown = text_spec(
            ClipPlatform::Unknown,
            NativeFormat::Unknown("mime:text/plain;charset=utf-8".into()),
            Medium::Bytes,
            "本地".as_bytes().to_vec(),
        );
        assert_eq!(plain_text(&unknown).unwrap(), "本地");

        let wrong_win = text_spec(
            ClipPlatform::Windows,
            NativeFormat::WindowsRegistered("text".into()),
            Medium::HGlobal,
            b"nope".to_vec(),
        );
        assert!(matches!(
            plain_text(&wrong_win),
            Err(ClipboardError::ContentUnavailable(Unavailable::InvalidData))
        ));
    }

    #[test]
    fn unbound_slot_rejects_read() {
        let backend = portal_backend(Arc::new(PortalClipboardSlot::new())).unwrap();
        assert!(matches!(backend.read(), Err(ClipboardError::Platform(_))));
    }

    #[test]
    fn selection_read_failures_are_not_empty_clipboard() {
        let err = text_from_selection_reads(&[Err("nope".into()), Err("still nope".into())]);
        assert!(matches!(err, Err(ClipboardError::Platform(_))));
        assert_eq!(text_from_selection_reads(&[Ok(Vec::new())]).unwrap(), "");
        assert_eq!(
            text_from_selection_reads(&[Err("x".into()), Ok(b"hi".to_vec())]).unwrap(),
            "hi"
        );
    }
}
