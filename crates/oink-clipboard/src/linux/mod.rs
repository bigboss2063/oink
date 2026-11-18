//! Linux clipboard via `org.freedesktop.portal.Clipboard` (Plasma 6.4+).
//!
//! The portal does not create its own session: it extends the RemoteDesktop
//! session owned by `oink-input`. [`PortalClipboardSlot`] is filled after
//! `RequestClipboard` succeeds, and cleared when that session drops.
//!
//! Text, PNG, and local files (`text/uri-list` / GNOME copied-files). Transfers
//! use the MIME ownership protocol (`SetSelection` / `SelectionTransfer` /
//! `SelectionRead`). Own writes are filtered with `session_is_owner`.
//!
//! shortcut: virtual / on-demand files have no portal File Provider; only
//! materialized `local_path` roots are published. Upgrade if a Linux placeholder
//! API appears.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd as StdOwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ashpd::desktop::clipboard::{Clipboard, SelectionOwnerChanged, SetSelectionOptions};
use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::desktop::Session;
use futures_util::StreamExt;
use oink_foundation::DeviceId;
use oink_foundation::clipboard::{ClipData, ClipPlatform, ClipSnapshot, FileEntry, Sensitivity};
use oink_foundation::content::{
    ClipId, ContentItem, ContentSize, ItemId, Medium, NativeFormat, NodeId, OfferId, Representation,
    RepresentationData, RepresentationId, StandardFormat, Unavailable,
};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::Notify;

use crate::content::{self, Provider, Translator};
use crate::{ChangeHandler, ClipboardBackend, ClipboardError, NativeSpec};

// Pure DIB↔PNG codec shared with the Windows backend (no Win32).
#[path = "../windows/image.rs"]
mod windows_image;

const TEXT_UTF8: &str = "text/plain;charset=utf-8";
/// `CF_DIB`
const CF_DIB: u16 = 8;
/// `CF_DIBV5`
const CF_DIBV5: u16 = 17;
const TEXT_PLAIN: &str = "text/plain";
const TEXT_MIMES: &[&str] = &[TEXT_UTF8, TEXT_PLAIN];
const IMAGE_PNG: &str = "image/png";
const URI_LIST: &str = "text/uri-list";
const GNOME_COPIED: &str = "x-special/gnome-copied-files";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

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
    let offer = Arc::new(tokio::sync::Mutex::new(Offer::default()));
    let transfer = TransferServer::start(Arc::clone(&slot), Arc::clone(&offer))?;
    Ok(Box::new(LinuxClipboard {
        slot,
        last: Arc::new(tokio::sync::Mutex::new(Seen::default())),
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

#[derive(Clone, Default)]
struct Offer {
    text: Option<String>,
    png: Option<Vec<u8>>,
    uri_list: Option<String>,
}

impl Offer {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.png.is_none() && self.uri_list.is_none()
    }

    fn mime_types(&self) -> Vec<&str> {
        let mut mimes = Vec::new();
        if self.text.is_some() {
            mimes.extend_from_slice(TEXT_MIMES);
        }
        if self.png.is_some() {
            mimes.push(IMAGE_PNG);
        }
        if self.uri_list.is_some() {
            mimes.push(URI_LIST);
            mimes.push(GNOME_COPIED);
        }
        mimes
    }

    fn payload(&self, mime: &str) -> Option<Vec<u8>> {
        if mime == TEXT_UTF8 || mime == TEXT_PLAIN {
            return self.text.as_ref().map(|text| text.as_bytes().to_vec());
        }
        if mime == IMAGE_PNG {
            return self.png.clone();
        }
        if mime == URI_LIST {
            return self.uri_list.as_ref().map(|uris| uris.as_bytes().to_vec());
        }
        if mime == GNOME_COPIED {
            return self.uri_list.as_ref().map(|uris| gnome_copied_files(uris).into_bytes());
        }
        None
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Seen {
    text: String,
    png: Option<u64>,
    uris: String,
}

impl Seen {
    fn from_selection(text: &str, png: Option<&[u8]>, paths: &[PathBuf]) -> Self {
        Self {
            text: text.to_owned(),
            png: png.map(digest),
            uris: encode_uri_list(paths),
        }
    }
}

struct LinuxClipboard {
    slot: Arc<PortalClipboardSlot>,
    last: Arc<tokio::sync::Mutex<Seen>>,
    /// Content we currently advertise via `SetSelection`, served on `SelectionTransfer`.
    offer: Arc<tokio::sync::Mutex<Offer>>,
    change_count: Arc<AtomicU64>,
    watcher: Mutex<Option<Watcher>>,
    /// Serves `SelectionTransfer` for our `SetSelection` even when watch is off.
    transfer: Mutex<Option<TransferServer>>,
    watch_paused: Arc<AtomicBool>,
}

impl LinuxClipboard {
    async fn read_selection_async(&self) -> Result<(String, Option<Vec<u8>>, Vec<PathBuf>), ClipboardError> {
        let bound = self.slot.get()?;
        read_selection(&bound.clipboard, bound.session.as_ref()).await
    }

    async fn publish_async(&self, offer: Offer) -> Result<u64, ClipboardError> {
        if offer.is_empty() {
            return Err(ClipboardError::Unsupported);
        }
        let bound = self.slot.get()?;
        let mimes = offer.mime_types();
        {
            let mut slot = self.offer.lock().await;
            *slot = offer.clone();
        }
        bound
            .clipboard
            .set_selection(
                bound.session.as_ref(),
                SetSelectionOptions::default().set_mime_types(&mimes),
            )
            .await
            .map_err(|e| ClipboardError::Platform(format!("SetSelection：{e}")))?;
        let mut last = self.last.lock().await;
        *last = Seen {
            text: offer.text.clone().unwrap_or_default(),
            png: offer.png.as_deref().map(digest),
            uris: offer.uri_list.clone().unwrap_or_default(),
        };
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

fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
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

async fn selection_read_bytes(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    mime: &str,
) -> Result<Vec<u8>, String> {
    match clipboard.selection_read(session, mime).await {
        Ok(fd) => read_all_fd(fd).map_err(|error| error.to_string()),
        Err(error) => {
            tracing::trace!(%error, mime, "SelectionRead mime unavailable");
            Err(error.to_string())
        }
    }
}

async fn read_plain_text(clipboard: &Clipboard, session: &Session<RemoteDesktop>) -> Result<String, ClipboardError> {
    let mut results = Vec::with_capacity(TEXT_MIMES.len());
    for mime in TEXT_MIMES {
        results.push(selection_read_bytes(clipboard, session, mime).await);
    }
    text_from_selection_reads(&results)
}

async fn read_selection(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
) -> Result<(String, Option<Vec<u8>>, Vec<PathBuf>), ClipboardError> {
    let text_result = read_plain_text(clipboard, session).await;
    let png = match selection_read_bytes(clipboard, session, IMAGE_PNG).await {
        Ok(bytes) if bytes.starts_with(PNG_MAGIC) => Some(bytes),
        _ => None,
    };
    let paths = read_file_paths(clipboard, session).await;
    match text_result {
        Ok(text) => Ok((text, png, paths)),
        // Image/file-only pasteboards have no text MIME; keep those usable.
        Err(_) if png.is_some() || !paths.is_empty() => Ok((String::new(), png, paths)),
        // Text-only read failure must not look like an empty clipboard.
        Err(error) => Err(error),
    }
}

async fn read_file_paths(clipboard: &Clipboard, session: &Session<RemoteDesktop>) -> Vec<PathBuf> {
    if let Ok(bytes) = selection_read_bytes(clipboard, session, URI_LIST).await {
        let paths = parse_uri_list(&bytes);
        if !paths.is_empty() {
            return paths;
        }
    }
    if let Ok(bytes) = selection_read_bytes(clipboard, session, GNOME_COPIED).await {
        return parse_gnome_copied(&bytes);
    }
    Vec::new()
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

fn interesting_mimes(mimes: &[String]) -> bool {
    mimes.iter().any(|mime| {
        mime.starts_with("text/plain")
            || mime.eq_ignore_ascii_case(IMAGE_PNG)
            || mime.eq_ignore_ascii_case(URI_LIST)
            || mime.eq_ignore_ascii_case(GNOME_COPIED)
    })
}

fn is_top_level(entry: &FileEntry) -> bool {
    !entry.path.contains('/')
}

fn is_png_encoding(format: &NativeFormat) -> bool {
    match format {
        NativeFormat::Mac(name) => name == "public.png",
        NativeFormat::WindowsRegistered(name) => {
            name.eq_ignore_ascii_case("PNG")
                || name.split(';').next().is_some_and(|mime| {
                    matches!(mime.trim().to_ascii_lowercase().as_str(), "image/png" | "image/x-png")
                })
        }
        NativeFormat::Unknown(name) => {
            let name = name.trim();
            name.eq_ignore_ascii_case("mime:image/png")
                || name.eq_ignore_ascii_case("mime:image/x-png")
                || name.eq_ignore_ascii_case("image/png")
        }
        NativeFormat::WindowsPredefined(_) => false,
    }
}

fn percent_encode_path_byte(byte: u8, out: &mut String) {
    match byte {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
        b'/' => out.push('/'),
        _ => {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
}

fn file_url(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let mut url = String::from("file://");
    if path.as_os_str().is_empty() {
        return None;
    }
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => {
                url.push('/');
                for byte in part.as_encoded_bytes() {
                    percent_encode_path_byte(*byte, &mut url);
                }
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    if url == "file://" {
        url.push('/');
    }
    Some(url)
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path_part = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (host, path) = rest.split_once('/')?;
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        format!("/{path}")
    };
    let decoded = percent_decode(&path_part)?;
    let path = PathBuf::from(decoded);
    path.is_absolute().then_some(path)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                out.push(value);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn encode_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        let Some(url) = file_url(path) else {
            continue;
        };
        out.push_str(&url);
        out.push_str("\r\n");
    }
    out
}

fn gnome_copied_files(uri_list: &str) -> String {
    let mut out = String::from("copy\n");
    for line in uri_list.lines() {
        let line = line.trim().trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim().trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(file_url_to_path)
        .collect()
}

fn parse_gnome_copied(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().map(|line| line.trim().trim_end_matches('\r'));
    match lines.next() {
        Some("copy" | "cut" | "link") => {}
        Some(first) => {
            return std::iter::once(first)
                .chain(lines)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .filter_map(file_url_to_path)
                .collect();
        }
        None => return Vec::new(),
    }
    lines
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(file_url_to_path)
        .collect()
}

fn uri_list_from_entries(entries: &[FileEntry]) -> Result<String, ClipboardError> {
    let tops: Vec<_> = entries.iter().filter(|entry| is_top_level(entry)).collect();
    if tops.is_empty() {
        return Err(ClipboardError::Unsupported);
    }
    let paths: Vec<_> = tops.iter().filter_map(|entry| entry.local_path.clone()).collect();
    if paths.len() != tops.len() {
        return Err(ClipboardError::FileProviderUnavailable);
    }
    let encoded = encode_uri_list(&paths);
    if encoded.is_empty() {
        return Err(ClipboardError::Unsupported);
    }
    Ok(encoded)
}

fn local_uri_list(spec: &NativeSpec) -> Result<Option<String>, ClipboardError> {
    if spec.files.is_empty() {
        return Ok(None);
    }
    // Portal clipboard is MIME+fd only. Callers must materialize first.
    if spec.file_contents.is_some() {
        return Err(ClipboardError::FileProviderUnavailable);
    }
    Ok(Some(uri_list_from_entries(&spec.files)?))
}

/// Relative clipboard paths must stay inside the batch directory.
fn batch_dest(batch: &Path, relative: &str) -> Result<PathBuf, ClipboardError> {
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ClipboardError::FileProviderUnavailable);
    }
    let dest = batch.join(relative);
    if !dest.starts_with(batch) {
        return Err(ClipboardError::FileProviderUnavailable);
    }
    Ok(dest)
}

/// Download remote/virtual files into `~/.cache/oink/Received/…` so portal can
/// advertise ordinary `file://` URIs. Failure leaves the agent free to retry
/// non-file companions via `FileProviderUnavailable`.
fn materialize_files(spec: &mut NativeSpec) -> Result<(), ClipboardError> {
    materialize_files_into(spec, &oink_foundation::paths::cache_dir().join("Received"))
}

fn materialize_files_into(spec: &mut NativeSpec, root: &Path) -> Result<(), ClipboardError> {
    if spec.files.is_empty() {
        return Ok(());
    }
    if spec
        .files
        .iter()
        .filter(|entry| is_top_level(entry))
        .all(|entry| entry.local_path.is_some())
    {
        spec.file_contents = None;
        return Ok(());
    }
    let Some(contents) = spec.file_contents.clone() else {
        return Err(ClipboardError::FileProviderUnavailable);
    };
    let entries = match contents.descriptor_entries() {
        Ok(Some(entries)) if !entries.is_empty() => entries,
        Ok(_) => spec.files.clone(),
        Err(error) => return Err(materialize_error(error)),
    };
    fs_create_dir_all(root)?;
    let batch = root.join(format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        std::process::id()
    ));
    fs_create_dir_all(&batch)?;
    let written = (|| {
        for (index, entry) in entries.iter().enumerate() {
            let dest = batch_dest(&batch, &entry.path)?;
            if entry.is_dir {
                fs_create_dir_all(&dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs_create_dir_all(parent)?;
            }
            let mut reader = contents.open(index).map_err(materialize_error)?;
            let mut file = std::fs::File::create(&dest).map_err(|error| {
                ClipboardError::Platform(format!("写入收到的文件失败：{error}"))
            })?;
            io::copy(&mut reader, &mut file).map_err(|error| {
                ClipboardError::Platform(format!("写入收到的文件失败：{error}"))
            })?;
        }
        Ok(())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&batch);
        return Err(error);
    }
    for entry in &mut spec.files {
        if is_top_level(entry) {
            entry.local_path = Some(batch.join(&entry.path));
        }
    }
    spec.file_contents = None;
    Ok(())
}

/// Cancel/busy stay distinct; every other materialize failure must look like
/// missing File Provider so the agent retries text/PNG companions.
fn materialize_error(error: io::Error) -> ClipboardError {
    match read_error(error) {
        ClipboardError::Busy => ClipboardError::Busy,
        _ => ClipboardError::FileProviderUnavailable,
    }
}

fn fs_create_dir_all(path: &Path) -> Result<(), ClipboardError> {
    std::fs::create_dir_all(path).map_err(|error| ClipboardError::Platform(format!("创建目录失败：{error}")))
}

fn offer_from_items(items: &[ClipData]) -> Result<Offer, ClipboardError> {
    let text = items.iter().find_map(|item| match item {
        ClipData::Text(text) => Some(text.clone()),
        _ => None,
    });
    let png = items.iter().find_map(|item| match item {
        ClipData::Png(bytes) if bytes.starts_with(PNG_MAGIC) => Some(bytes.clone()),
        _ => None,
    });
    let uri_list = items
        .iter()
        .find_map(|item| match item {
            ClipData::Files(entries) => Some(uri_list_from_entries(entries)),
            _ => None,
        })
        .transpose()?;
    let offer = Offer { text, png, uri_list };
    if offer.is_empty() {
        Err(ClipboardError::Unsupported)
    } else {
        Ok(offer)
    }
}

fn offer_from_spec(spec: &NativeSpec) -> Result<Offer, ClipboardError> {
    let uri_list = local_uri_list(spec)?;
    let text = plain_text(spec).ok();
    let png = png_bytes(spec).ok();
    let offer = Offer { text, png, uri_list };
    if offer.is_empty() {
        Err(ClipboardError::Unsupported)
    } else {
        Ok(offer)
    }
}

fn file_entries(paths: &[PathBuf]) -> Vec<FileEntry> {
    paths
        .iter()
        .filter_map(|path| {
            let meta = std::fs::symlink_metadata(path).ok()?;
            let name = path.file_name()?.to_str()?.to_owned();
            Some(FileEntry {
                path: name,
                is_dir: meta.is_dir(),
                size: if meta.is_file() { meta.len() } else { 0 },
                modified_ms: meta.modified().ok().and_then(|time| {
                    time.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|duration| duration.as_millis() as i64)
                }),
                local_path: Some(path.clone()),
            })
        })
        .collect()
}

impl ClipboardBackend for LinuxClipboard {
    fn change_count(&self) -> u64 {
        self.change_count.load(Ordering::Acquire)
    }

    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        let (text, png, paths) = block_on(self.read_selection_async())?;
        let mut items = Vec::new();
        let files = file_entries(&paths);
        if !files.is_empty() {
            items.push(ClipData::Files(files));
        }
        if !text.is_empty() {
            items.push(ClipData::Text(text));
        }
        if let Some(png) = png {
            items.push(ClipData::Png(png));
        }
        Ok(ClipSnapshot {
            items,
            sensitivity: Sensitivity::Normal,
            change_count: self.change_count(),
        })
    }

    fn capture(&self, _data: &std::path::Path) -> Result<crate::CapturedClipboard, ClipboardError> {
        let count = self.change_count();
        let (text, png, paths) = block_on(self.read_selection_async())?;
        if self.change_count() != count {
            return Err(ClipboardError::Busy);
        }
        let slot = Arc::clone(&self.slot);
        let counts = Arc::clone(&self.change_count);
        let expected_text = text.clone();
        let expected_png = png.clone();
        let provider = serve_selection(
            count,
            text,
            png,
            paths,
            move || {
                if counts.load(Ordering::Acquire) != count {
                    return Err(content::clipboard::changed());
                }
                let bound = slot.get().map_err(|e| io::Error::other(e.to_string()))?;
                let (current_text, current_png, _) =
                    block_on(read_selection(&bound.clipboard, bound.session.as_ref()))
                        .map_err(|e| io::Error::other(e.to_string()))?;
                if counts.load(Ordering::Acquire) != count {
                    return Err(content::clipboard::changed());
                }
                if current_text != expected_text || current_png != expected_png {
                    return Err(content::clipboard::changed());
                }
                Ok((current_text, current_png))
            },
        )
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
        let offer = offer_from_items(items)?;
        block_on(self.publish_async(offer))
    }

    fn write_native(&self, spec: NativeSpec) -> Result<u64, ClipboardError> {
        self.write_native_guarded(spec, Arc::new(|| Ok(())))
    }

    fn write_native_guarded(&self, spec: NativeSpec, guard: content::ReaderGuard) -> Result<u64, ClipboardError> {
        self.write_native_cancellable(spec, guard, Arc::new(|| Ok(())))
    }

    fn write_native_cancellable(
        &self,
        mut spec: NativeSpec,
        guard: content::ReaderGuard,
        current: content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        current().map_err(|_| ClipboardError::Busy)?;
        // Materialize virtual files into Received/; if that is impossible, keep
        // FileProviderUnavailable so the agent can retry text/PNG companions.
        if let Err(error) = materialize_files(&mut spec) {
            tracing::warn!(%error, files = spec.files.len(), "linux received materialize failed");
            return Err(error);
        }
        let offer = match offer_from_spec(&spec) {
            Ok(offer) => offer,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    files = spec.files.len(),
                    ?spec.sensitivity,
                    "linux clipboard publish rejected"
                );
                return Err(error);
            }
        };
        guard().map_err(|_| ClipboardError::Busy)?;
        publishable_sensitivity(spec.sensitivity)?;
        block_on(self.publish_async(offer))
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
        last: Arc<tokio::sync::Mutex<Seen>>,
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
    fn start(slot: Arc<PortalClipboardSlot>, offer: Arc<tokio::sync::Mutex<Offer>>) -> Result<Self, ClipboardError> {
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
    last: Arc<tokio::sync::Mutex<Seen>>,
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
    offer: Arc<tokio::sync::Mutex<Offer>>,
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
    last: &tokio::sync::Mutex<Seen>,
    change_count: &AtomicU64,
) -> bool {
    if change.session_is_owner() == Some(true) {
        return false;
    }
    let mimes = change.mime_types();
    if mimes.is_empty() {
        let mut last = last.lock().await;
        if *last == Seen::default() {
            return false;
        }
        *last = Seen::default();
        change_count.fetch_add(1, Ordering::AcqRel);
        return true;
    }
    if !interesting_mimes(mimes) {
        return false;
    }
    let Ok((text, png, paths)) = read_selection(clipboard, session).await else {
        // Read failure must not look like an empty clipboard.
        return false;
    };
    let seen = Seen::from_selection(&text, png.as_deref(), &paths);
    let mut last = last.lock().await;
    if seen == *last {
        return false;
    }
    *last = seen;
    change_count.fetch_add(1, Ordering::AcqRel);
    true
}

async fn serve_transfer(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    mime: &str,
    serial: u32,
    offer: &tokio::sync::Mutex<Offer>,
) {
    let payload = offer.lock().await.payload(mime);
    let Some(bytes) = payload else {
        let _ = clipboard.selection_write_done(session, serial, false).await;
        return;
    };
    let Ok(fd) = clipboard.selection_write(session, serial).await else {
        let _ = clipboard.selection_write_done(session, serial, false).await;
        return;
    };
    let ok = write_all_fd(fd, &bytes).is_ok();
    let _ = clipboard.selection_write_done(session, serial, ok).await;
}

struct PortalTranslator;
impl Translator for PortalTranslator {
    fn standard(&self, representation: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        content::decode_standard(representation, bytes)
    }
}

fn empty_inventory(count: u64) -> crate::model::Inventory {
    crate::model::Inventory {
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
    }
}

fn selection_inventory(count: u64, text: &str, png: Option<&[u8]>) -> crate::model::Inventory {
    let mut inventory = empty_inventory(count);
    let mut next_item = 1u32;
    let mut next_rep = 1u32;
    if !text.is_empty() {
        inventory.items.push(ContentItem {
            id: ItemId(next_item),
            virtual_files: false,
            file: None,
            representations: vec![Representation {
                id: RepresentationId(next_rep),
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
        next_item += 1;
        next_rep += 1;
    }
    if let Some(png) = png {
        inventory.items.push(ContentItem {
            id: ItemId(next_item),
            virtual_files: false,
            file: None,
            representations: vec![Representation {
                id: RepresentationId(next_rep),
                format: NativeFormat::Unknown("mime:image/png".into()),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: Some(StandardFormat::Png),
                data: RepresentationData::Reader {
                    size: ContentSize::Known(png.len() as u64),
                },
            }],
        });
    }
    inventory
}

fn serve_selection(
    count: u64,
    text: String,
    png: Option<Vec<u8>>,
    paths: Vec<PathBuf>,
    read: impl Fn() -> io::Result<(String, Option<Vec<u8>>)> + Send + Sync + 'static,
) -> io::Result<Arc<dyn Provider>> {
    let inventory = selection_inventory(count, &text, png.as_deref());
    let text_id = inventory
        .items
        .iter()
        .find(|item| item.representations.iter().any(|rep| rep.standard == Some(StandardFormat::Text)))
        .and_then(|item| item.representations.first().map(|rep| rep.id));
    let png_id = inventory
        .items
        .iter()
        .find(|item| item.representations.iter().any(|rep| rep.standard == Some(StandardFormat::Png)))
        .and_then(|item| item.representations.first().map(|rep| rep.id));
    let expected_text = text;
    let expected_png = png;
    let files = if paths.is_empty() {
        None
    } else {
        let roots = paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| (NodeId(index as u64 + 1), path))
            .collect();
        Some(content::namespace::Local::clipboard(roots)? as Arc<dyn content::namespace::Namespace>)
    };
    let provider = content::clipboard::Clipboard::new(
        inventory,
        Box::new(move |id| {
            let (current_text, current_png) = read()?;
            if Some(id) == text_id {
                if current_text != expected_text {
                    return Err(content::clipboard::changed());
                }
                return Ok(Box::new(std::io::Cursor::new(current_text.into_bytes())));
            }
            if Some(id) == png_id {
                let Some(bytes) = current_png else {
                    return Err(content::clipboard::changed());
                };
                if Some(&bytes) != expected_png.as_ref() {
                    return Err(content::clipboard::changed());
                }
                return Ok(Box::new(std::io::Cursor::new(bytes)));
            }
            Err(content::unavailable(Unavailable::NoSuchItem))
        }),
        Arc::new(|| Ok(())),
        Arc::new(PortalTranslator),
        files,
    )?;
    Ok(provider)
}

/// Portal SetSelection cannot mark sensitivity. Concealed stays fail-closed so
/// secrets are not dropped into ordinary clipboard history. Transient /
/// AutoGenerated (screenshots, etc.) still publish — otherwise Linux never
/// receives the most common image copies.
fn publishable_sensitivity(sensitivity: Sensitivity) -> Result<(), ClipboardError> {
    match sensitivity {
        Sensitivity::Normal | Sensitivity::Transient | Sensitivity::AutoGenerated => Ok(()),
        Sensitivity::Concealed => Err(ClipboardError::Unsupported),
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

fn is_windows_dib(format: &NativeFormat) -> bool {
    match format {
        NativeFormat::WindowsPredefined(CF_DIB) | NativeFormat::WindowsPredefined(CF_DIBV5) => true,
        NativeFormat::WindowsRegistered(name) => {
            name.eq_ignore_ascii_case("Dib") || name.eq_ignore_ascii_case("DeviceIndependentBitmap")
        }
        _ => false,
    }
}

fn png_bytes(spec: &NativeSpec) -> Result<Vec<u8>, ClipboardError> {
    let target = crate::reconstruction_target();
    let mut candidates: Vec<&Representation> = spec
        .inventory
        .wanted(target)
        .filter(|representation| representation.standard == Some(StandardFormat::Png))
        .filter(|representation| {
            !matches!(
                representation.data,
                RepresentationData::Unavailable(_) | RepresentationData::File(_) | RepresentationData::Promise(..)
            )
        })
        .collect();
    if candidates.is_empty() {
        return Err(ClipboardError::Unsupported);
    }
    // Prefer already-encoded PNG; fall back to CF_DIB / CF_DIBV5 conversion.
    candidates.sort_by_key(|representation| {
        (
            !is_png_encoding(&representation.format),
            !is_windows_dib(&representation.format),
        )
    });
    let mut saw = false;
    let mut last_error = None;
    for representation in candidates {
        saw = true;
        let dib = is_windows_dib(&representation.format);
        if spec.inventory.platform == ClipPlatform::Windows
            && !is_png_encoding(&representation.format)
            && !dib
        {
            continue;
        }
        let mut reader = match spec.contents.open(representation.id) {
            Ok(reader) => reader,
            Err(error) => {
                last_error = Some(read_error(error));
                continue;
            }
        };
        let bytes = match content::read_all(&mut reader) {
            Ok(bytes) => bytes,
            Err(error) => {
                last_error = Some(read_error(error));
                continue;
            }
        };
        if bytes.starts_with(PNG_MAGIC) {
            return Ok(bytes);
        }
        if dib {
            match windows_image::dib_to_png(&bytes) {
                Ok(png) => return Ok(png),
                Err(error) => {
                    last_error = Some(ClipboardError::ContentUnavailable(error.reason()));
                    continue;
                }
            }
        }
    }
    Err(last_error.unwrap_or(if saw {
        ClipboardError::ContentUnavailable(Unavailable::InvalidData)
    } else {
        ClipboardError::Unsupported
    }))
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

    fn tiny_png() -> Vec<u8> {
        // 1x1 transparent PNG.
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00,
            0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01,
            0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn text_offer_is_portable_utf8_for_mac_and_windows() {
        let provider = serve_selection(3, "你好".into(), None, Vec::new(), || Ok(("你好".into(), None))).unwrap();
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
    fn png_offer_is_portable_for_mac_and_windows() {
        let png = tiny_png();
        let provider = serve_selection(4, String::new(), Some(png.clone()), Vec::new(), {
            let png = png.clone();
            move || Ok((String::new(), Some(png.clone())))
        })
        .unwrap();
        let inventory = provider.inventory();
        assert!(inventory.validate().is_ok());
        for target in [ClipPlatform::Mac, ClipPlatform::Windows] {
            let representation = inventory.wanted(target).next().expect("png");
            assert_eq!(representation.standard, Some(StandardFormat::Png));
            let mut reader = content::open_for(provider.as_ref(), representation.id, target).unwrap();
            assert_eq!(content::read_all(&mut reader).unwrap(), png);
        }
    }

    #[test]
    fn empty_clipboard_has_no_items() {
        let provider = serve_selection(1, String::new(), None, Vec::new(), || Ok((String::new(), None))).unwrap();
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
    fn only_concealed_sensitivity_is_rejected() {
        assert!(publishable_sensitivity(Sensitivity::Normal).is_ok());
        assert!(publishable_sensitivity(Sensitivity::Transient).is_ok());
        assert!(publishable_sensitivity(Sensitivity::AutoGenerated).is_ok());
        assert!(matches!(
            publishable_sensitivity(Sensitivity::Concealed),
            Err(ClipboardError::Unsupported)
        ));
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
            Arc::new(PortalTranslator)
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

    fn png_spec(platform: ClipPlatform, format: NativeFormat, bytes: Vec<u8>) -> NativeSpec {
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
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: Some(StandardFormat::Png),
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
    fn png_bytes_accepts_portable_and_native_png_encodings() {
        let png = tiny_png();
        let portable = png_spec(
            ClipPlatform::Mac,
            NativeFormat::Unknown("mime:image/png".into()),
            png.clone(),
        );
        assert_eq!(png_bytes(&portable).unwrap(), png);

        let win_png = png_spec(
            ClipPlatform::Windows,
            NativeFormat::WindowsRegistered("PNG".into()),
            png.clone(),
        );
        assert_eq!(png_bytes(&win_png).unwrap(), png);

        let bad_dib = png_spec(
            ClipPlatform::Windows,
            NativeFormat::WindowsPredefined(CF_DIB),
            b"BM not a png".to_vec(),
        );
        assert!(matches!(
            png_bytes(&bad_dib),
            Err(ClipboardError::ContentUnavailable(Unavailable::InvalidData))
        ));

        // Minimal 1×1 BI_RGB 24-bit bottom-up DIB (BITMAPINFOHEADER + BGR pixel + pad).
        let mut dib = vec![
            40, 0, 0, 0, // biSize
            1, 0, 0, 0, // biWidth
            1, 0, 0, 0, // biHeight
            1, 0, // biPlanes
            24, 0, // biBitCount
            0, 0, 0, 0, // BI_RGB
            4, 0, 0, 0, // biSizeImage
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        dib.extend_from_slice(&[0x10, 0x20, 0x30, 0x00]); // BGR + pad
        let from_dib = png_spec(
            ClipPlatform::Windows,
            NativeFormat::WindowsPredefined(CF_DIB),
            dib,
        );
        let converted = png_bytes(&from_dib).unwrap();
        assert!(converted.starts_with(PNG_MAGIC));
    }

    #[test]
    fn file_url_roundtrip_encodes_spaces() {
        let path = PathBuf::from("/tmp/hello world.png");
        let url = file_url(&path).unwrap();
        assert_eq!(url, "file:///tmp/hello%20world.png");
        assert_eq!(file_url_to_path(&url), Some(path));
    }

    #[test]
    fn uri_list_and_gnome_formats_parse_paths() {
        let list = "file:///tmp/a\r\n# comment\r\nfile:///tmp/b\r\n";
        assert_eq!(
            parse_uri_list(list.as_bytes()),
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        let gnome = "copy\nfile:///tmp/a\nfile:///tmp/b\n";
        assert_eq!(
            parse_gnome_copied(gnome.as_bytes()),
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        assert_eq!(
            gnome_copied_files(list),
            "copy\nfile:///tmp/a\nfile:///tmp/b\n"
        );
    }

    #[test]
    fn offer_from_items_requires_local_paths_for_files() {
        let ready = offer_from_items(&[ClipData::Files(vec![FileEntry {
            path: "a.txt".into(),
            is_dir: false,
            size: 1,
            modified_ms: None,
            local_path: Some(PathBuf::from("/tmp/a.txt")),
        }])])
        .unwrap();
        assert!(ready.uri_list.unwrap().contains("file:///tmp/a.txt"));

        let missing = offer_from_items(&[ClipData::Files(vec![FileEntry {
            path: "a.txt".into(),
            is_dir: false,
            size: 1,
            modified_ms: None,
            local_path: None,
        }])]);
        assert!(matches!(missing, Err(ClipboardError::FileProviderUnavailable)));
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

    #[test]
    fn interesting_mimes_include_png_and_files() {
        assert!(interesting_mimes(&["image/png".into()]));
        assert!(interesting_mimes(&["text/uri-list".into()]));
        assert!(interesting_mimes(&["x-special/gnome-copied-files".into()]));
        assert!(!interesting_mimes(&["text/html".into()]));
    }

    #[test]
    fn batch_dest_rejects_traversal() {
        let batch = PathBuf::from("/tmp/oink-batch");
        assert!(batch_dest(&batch, "a.txt").is_ok());
        assert!(batch_dest(&batch, "dir/a.txt").is_ok());
        assert!(matches!(
            batch_dest(&batch, "../escape"),
            Err(ClipboardError::FileProviderUnavailable)
        ));
        assert!(matches!(
            batch_dest(&batch, "a/../../escape"),
            Err(ClipboardError::FileProviderUnavailable)
        ));
    }

    struct MemoryFiles {
        entries: Vec<FileEntry>,
        bodies: Vec<Vec<u8>>,
        fail_at: Option<usize>,
    }

    impl crate::FileContents for MemoryFiles {
        fn descriptor_entries(&self) -> io::Result<Option<Vec<FileEntry>>> {
            Ok(Some(self.entries.clone()))
        }

        fn open(&self, index: usize) -> io::Result<crate::FileReader> {
            if self.fail_at == Some(index) {
                return Err(io::Error::other("forced open failure"));
            }
            let bytes = self
                .bodies
                .get(index)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(Box::new(std::io::Cursor::new(bytes)))
        }
    }

    fn file_spec(entries: Vec<FileEntry>, bodies: Vec<Vec<u8>>, fail_at: Option<usize>) -> NativeSpec {
        let inventory = empty_inventory(1);
        NativeSpec {
            inventory: inventory.clone(),
            contents: Arc::new(BytesOffer {
                inventory,
                bytes: Vec::new(),
            }),
            files: entries
                .iter()
                .filter(|entry| is_top_level(entry))
                .cloned()
                .collect(),
            file_contents: Some(Arc::new(MemoryFiles {
                entries,
                bodies,
                fail_at,
            })),
            sensitivity: Sensitivity::Normal,
        }
    }

    #[test]
    fn materialize_files_sets_local_paths_under_batch() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = file_spec(
            vec![
                FileEntry {
                    path: "a.txt".into(),
                    is_dir: false,
                    size: 3,
                    modified_ms: None,
                    local_path: None,
                },
                FileEntry {
                    path: "dir".into(),
                    is_dir: true,
                    size: 0,
                    modified_ms: None,
                    local_path: None,
                },
                FileEntry {
                    path: "dir/b.txt".into(),
                    is_dir: false,
                    size: 3,
                    modified_ms: None,
                    local_path: None,
                },
            ],
            vec![b"aaa".to_vec(), Vec::new(), b"bbb".to_vec()],
            None,
        );
        materialize_files_into(&mut spec, root.path()).unwrap();
        assert!(spec.file_contents.is_none());
        let a = spec.files.iter().find(|e| e.path == "a.txt").unwrap();
        let dir = spec.files.iter().find(|e| e.path == "dir").unwrap();
        let a_path = a.local_path.as_ref().unwrap();
        let dir_path = dir.local_path.as_ref().unwrap();
        assert!(a_path.starts_with(root.path()));
        assert!(dir_path.starts_with(root.path()));
        assert_eq!(std::fs::read(a_path).unwrap(), b"aaa");
        assert_eq!(std::fs::read(dir_path.join("b.txt")).unwrap(), b"bbb");
    }

    #[test]
    fn materialize_files_removes_batch_when_open_fails() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = file_spec(
            vec![
                FileEntry {
                    path: "a.txt".into(),
                    is_dir: false,
                    size: 1,
                    modified_ms: None,
                    local_path: None,
                },
                FileEntry {
                    path: "b.txt".into(),
                    is_dir: false,
                    size: 1,
                    modified_ms: None,
                    local_path: None,
                },
            ],
            vec![b"a".to_vec(), b"b".to_vec()],
            Some(1),
        );
        let err = materialize_files_into(&mut spec, root.path()).unwrap_err();
        assert!(matches!(err, ClipboardError::FileProviderUnavailable));
        assert!(spec.files.iter().all(|entry| entry.local_path.is_none()));
        assert!(
            std::fs::read_dir(root.path()).unwrap().next().is_none(),
            "failed batch must be deleted"
        );
    }
}
