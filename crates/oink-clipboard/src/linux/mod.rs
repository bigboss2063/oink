//! KDE Plasma text clipboard via Klipper D-Bus (`org.kde.klipper`).
//!
//! Text only for now — images and files are not published. Klipper is the
//! session clipboard service on Plasma and supports background read/write
//! without a focused surface. Changes arrive as `clipboardHistoryUpdated`; the
//! signal has no payload, so each notification reads the current text once.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use oink_foundation::DeviceId;
use oink_foundation::clipboard::{ClipData, ClipPlatform, ClipSnapshot, Sensitivity};
use oink_foundation::content::{
    ClipId, ContentItem, ContentSize, ItemId, Medium, NativeFormat, OfferId, Representation, RepresentationData,
    RepresentationId, StandardFormat, Unavailable,
};
use tokio::runtime::{Handle, Runtime};
use zbus::Connection;
use zbus::export::futures_core::Stream;
use zbus::proxy;

use crate::content::{self, Provider, Translator};
use crate::{ChangeHandler, ClipboardBackend, ClipboardError, NativeSpec};

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
    // A nested `Runtime::block_on` panics, and `block_in_place` also panics when
    // the current thread is driving `block_on` rather than a multi-thread worker.
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

#[proxy(
    default_service = "org.kde.klipper",
    default_path = "/klipper",
    interface = "org.kde.klipper.klipper"
)]
trait Klipper {
    #[zbus(name = "getClipboardContents")]
    fn get_clipboard_contents(&self) -> zbus::Result<String>;

    #[zbus(name = "setClipboardContents")]
    fn set_clipboard_contents(&self, text: &str) -> zbus::Result<()>;

    /// Klipper's history signal is camelCase, not the macro's default PascalCase.
    #[zbus(signal, name = "clipboardHistoryUpdated")]
    fn clipboard_history_updated(&self) -> zbus::Result<()>;
}

pub(crate) fn backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if !session.eq_ignore_ascii_case("wayland") && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(ClipboardError::Platform(
            "需要 Wayland 会话（XDG_SESSION_TYPE=wayland 或 WAYLAND_DISPLAY）".into(),
        ));
    }
    let (klipper, current) = block_on(async {
        let conn = Connection::session()
            .await
            .map_err(|e| ClipboardError::Platform(format!("连接 session bus 失败：{e}")))?;
        let klipper = KlipperProxy::new(&conn)
            .await
            .map_err(|e| ClipboardError::Platform(format!("连接 Klipper 失败：{e}")))?;
        let current = klipper
            .get_clipboard_contents()
            .await
            .map_err(|e| ClipboardError::Platform(format!("读取 Klipper 剪贴板失败：{e}")))?;
        Ok::<_, ClipboardError>((klipper, current))
    })?;
    Ok(Box::new(LinuxClipboard {
        klipper,
        last: Arc::new(tokio::sync::Mutex::new(current)),
        change_count: Arc::new(AtomicU64::new(1)),
        watcher: Mutex::new(None),
        watch_paused: Arc::new(AtomicBool::new(false)),
    }))
}

struct LinuxClipboard {
    klipper: KlipperProxy<'static>,
    /// Last text attributed to a change count. Our own writes and the watcher
    /// both update it under this lock, so a write is never seen as external.
    last: Arc<tokio::sync::Mutex<String>>,
    change_count: Arc<AtomicU64>,
    watcher: Mutex<Option<Watcher>>,
    watch_paused: Arc<AtomicBool>,
}

impl LinuxClipboard {
    fn current_text(&self) -> Result<String, ClipboardError> {
        block_on(self.klipper.get_clipboard_contents())
            .map_err(|e| ClipboardError::Platform(format!("getClipboardContents：{e}")))
    }
}

impl ClipboardBackend for LinuxClipboard {
    fn change_count(&self) -> u64 {
        self.change_count.load(Ordering::Acquire)
    }

    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        let text = self.current_text()?;
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
        let text = self.current_text()?;
        if self.change_count() != count {
            return Err(ClipboardError::Busy);
        }
        let klipper = self.klipper.clone();
        let counts = Arc::clone(&self.change_count);
        let provider = serve_text(count, text, move || {
            if counts.load(Ordering::Acquire) != count {
                return Err(content::clipboard::changed());
            }
            let current =
                block_on(klipper.get_clipboard_contents()).map_err(|error| io::Error::other(error.to_string()))?;
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
        block_on(async {
            let mut last = self.last.lock().await;
            self.klipper
                .set_clipboard_contents(text)
                .await
                .map_err(|e| ClipboardError::Platform(format!("setClipboardContents：{e}")))?;
            text.clone_into(&mut *last);
            Ok(self.change_count.fetch_add(1, Ordering::AcqRel) + 1)
        })
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
                    self.klipper.clone(),
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
    wake: Arc<tokio::sync::Notify>,
    handlers: Arc<Mutex<Vec<ChangeHandler>>>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    fn start(
        klipper: KlipperProxy<'static>,
        last: Arc<tokio::sync::Mutex<String>>,
        change_count: Arc<AtomicU64>,
        paused: Arc<AtomicBool>,
        first: ChangeHandler,
    ) -> Result<Self, ClipboardError> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let handlers = Arc::new(Mutex::new(vec![first]));
        let thread_stop = Arc::clone(&stop);
        let thread_wake = Arc::clone(&wake);
        let thread_handlers = Arc::clone(&handlers);
        let thread = std::thread::Builder::new()
            .name("oink-clipboard-watch".into())
            .spawn(move || {
                runtime().block_on(run_watch(
                    klipper,
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

async fn run_watch(
    klipper: KlipperProxy<'static>,
    last: Arc<tokio::sync::Mutex<String>>,
    stop: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    paused: Arc<AtomicBool>,
    handlers: Arc<Mutex<Vec<ChangeHandler>>>,
    change_count: Arc<AtomicU64>,
) {
    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::Acquire) {
        let stream = tokio::select! {
            _ = wake.notified() => return,
            stream = klipper.receive_clipboard_history_updated() => stream,
        };
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "订阅 Klipper clipboardHistoryUpdated 失败");
                tokio::select! {
                    _ = wake.notified() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        backoff = Duration::from_millis(200);
        let mut stream = std::pin::pin!(stream);
        while !stop.load(Ordering::Acquire) {
            let signal = tokio::select! {
                _ = wake.notified() => return,
                signal = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)) => signal,
            };
            let Some(_signal) = signal else {
                break;
            };
            if paused.load(Ordering::Acquire) {
                continue;
            }
            if !observe_change(&klipper, &last, &change_count).await {
                continue;
            }
            let list = handlers.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for handler in list {
                handler();
            }
        }
    }
}

/// Read once per notification. The write path holds `last` across
/// `setClipboardContents`, so this cannot observe our own write before `last`
/// is updated.
async fn observe_change(
    klipper: &KlipperProxy<'_>,
    last: &tokio::sync::Mutex<String>,
    change_count: &AtomicU64,
) -> bool {
    let mut last = last.lock().await;
    let Ok(text) = klipper.get_clipboard_contents().await else {
        return false;
    };
    if text == *last {
        return false;
    }
    *last = text;
    change_count.fetch_add(1, Ordering::AcqRel);
    true
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

/// Klipper's D-Bus text API cannot mark Concealed/Transient/AutoGenerated.
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
    fn empty_clipboard_has_no_representations() {
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
    #[ignore = "需要正在运行的 Plasma 会话和 Klipper"]
    fn klipper_signal_reports_external_text_only() {
        let backend = backend().expect("klipper");
        let original = backend.read().expect("read").items;
        let _restore = Restore {
            backend: backend.as_ref(),
            original,
        };
        let seen = Arc::new(AtomicU64::new(0));
        let flag = Arc::clone(&seen);
        backend
            .watch(Arc::new(move || {
                flag.fetch_add(1, Ordering::AcqRel);
            }))
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let marker = format!("oink-signal-{}", std::process::id());
        backend
            .write(&[ClipData::Text(marker.clone())], Sensitivity::Normal)
            .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(seen.load(Ordering::Acquire), 0, "本机写入被当成了外部变化");

        let external = format!("{marker}-external");
        block_on(async {
            let conn = Connection::session().await.unwrap();
            let klipper = KlipperProxy::new(&conn).await.unwrap();
            klipper.set_clipboard_contents(&external).await.unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while seen.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(seen.load(Ordering::Acquire), 1, "clipboardHistoryUpdated 没有叫醒监听");
        let captured = backend.capture(std::path::Path::new("unused")).unwrap();
        let provider = captured.content.expect("provider");
        let mut reader = provider.open(RepresentationId(1)).unwrap();
        assert_eq!(content::read_all(&mut reader).unwrap(), external.as_bytes());
    }

    struct Restore<'a> {
        backend: &'a dyn ClipboardBackend,
        original: Vec<ClipData>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let text = self.original.iter().find_map(|item| match item {
                ClipData::Text(text) => Some(text.clone()),
                _ => None,
            });
            if let Some(text) = text {
                let _ = self.backend.write(&[ClipData::Text(text)], Sensitivity::Normal);
            }
        }
    }
}
