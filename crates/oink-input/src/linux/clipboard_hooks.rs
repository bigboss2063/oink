//! Callbacks so clipboard can attach to the RemoteDesktop session without a crate cycle.

use std::sync::Arc;

use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::desktop::Session;

/// Filled by the agent with [`oink_clipboard::PortalClipboardSlot`] bind/unbind.
pub struct ClipboardPortalHooks {
    pub bind: Box<dyn Fn(Clipboard, Arc<Session<RemoteDesktop>>) + Send + Sync>,
    pub unbind: Box<dyn Fn() + Send + Sync>,
}
