//! Windows device-change notifications via `IMMNotificationClient`.
//!
//! Ownership model (ADR-016):
//!
//! ```text
//! IMMNotificationClient (COM, MMDevice-notification thread)
//!   └─ does the minimum: translate args → DeviceEvent → try_send
//! playback owner (engine thread)
//!   └─ select!s on the event channel and decides what playback does
//! ```
//!
//! Hard rules from the audio-engineering skill:
//! - The COM callback must never block, allocate unboundedly, log expensively,
//!   open streams, or do playback work. It does one `try_send` and returns.
//! - The channel is **unbounded**: a blocking `send` inside a COM callback
//!   can deadlock against the engine thread (which owns playback), and
//!   dropping a device event is worse than growing the queue — the engine
//!   re-verifies device state against MMDevice before acting on anything.
//! - Nothing here touches the `OutputStream`. Reopening a stream is the
//!   playback owner's decision, on its own thread.
//!
//! Non-Windows platforms get a stub that reports "no notifications", so the
//! core compiles and the engine behaves deterministically.

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, IMMNotificationClient};

use crate::audio::output::DeviceState;

/// A device change observed from the OS. Deliberately small, owned, and
/// cheap to send: this is the notification boundary payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum DeviceEvent {
    /// The default render endpoint changed. `endpoint_id` is the new
    /// default, or `None` when there is no default device any more.
    DefaultChanged { endpoint_id: Option<String> },
    /// A render endpoint appeared.
    Added { endpoint_id: String },
    /// A render endpoint disappeared (unplugged, driver removed).
    Removed { endpoint_id: String },
    /// An endpoint changed state (active ⇄ disabled/unplugged/notpresent).
    StateChanged {
        endpoint_id: String,
        state: DeviceState,
    },
    /// Endpoint registration failed or was torn down; notifications are no
    /// longer reliable. Playback continues; the engine treats this as
    /// "device state unknown" and keeps polling state at stream open time.
    Unavailable { reason: String },
}

impl DeviceEvent {
    /// Endpoint this event concerns, if any.
    pub fn endpoint_id(&self) -> Option<&str> {
        match self {
            Self::DefaultChanged { endpoint_id } => endpoint_id.as_deref(),
            Self::Added { endpoint_id }
            | Self::Removed { endpoint_id }
            | Self::StateChanged { endpoint_id, .. } => Some(endpoint_id),
            Self::Unavailable { .. } => None,
        }
    }

    /// Whether this event means the endpoint can no longer be used.
    /// A removed endpoint, or one that went to a non-active state, is gone
    /// for playback purposes.
    pub fn makes_endpoint_unusable(&self) -> bool {
        match self {
            Self::Removed { .. } => true,
            Self::StateChanged { state, .. } => *state != DeviceState::Active,
            Self::DefaultChanged { endpoint_id } => endpoint_id.is_none(),
            Self::Added { .. } | Self::Unavailable { .. } => false,
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{DeviceEvent, DeviceState};
    use crossbeam_channel::Sender;
    use windows::core::{implement, PCWSTR};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    /// Translate a WASAPI notification into our owned, backend-neutral event.
    ///
    /// Pure and unit-tested in `tests` below: this is the boundary where
    /// COM/WASAPI details stop.
    pub(super) fn translate(
        event: DeviceEvent,
        flow: EDataFlow,
        role: ERole,
    ) -> Option<DeviceEvent> {
        // We only care about render/console: capture inputs and other roles
        // (communications, multimedia) are not LUMEN's playback path.
        if flow != eRender || role != eConsole {
            return None;
        }
        Some(event)
    }

    fn text(id: &PCWSTR) -> Option<String> {
        if id.is_null() {
            return None;
        }
        // SAFETY: the callback receives a valid PCWSTR for the call duration.
        unsafe { id.to_string().ok() }.filter(|s| !s.is_empty())
    }

    #[implement(IMMNotificationClient)]
    struct Notifier {
        sender: Sender<DeviceEvent>,
    }

    // SAFETY contract: these run on MMDevice's notification thread. Each
    // body does argument translation plus one non-blocking `try_send`.
    impl IMMNotificationClient_Impl for Notifier_Impl {
        fn OnDeviceStateChanged(
            &self,
            id: &PCWSTR,
            new_state: windows::Win32::Media::Audio::DEVICE_STATE,
        ) -> windows::core::Result<()> {
            if let Some(event) = translate(
                DeviceEvent::StateChanged {
                    endpoint_id: text(id).unwrap_or_default(),
                    state: DeviceState::from_raw(new_state.0),
                },
                eRender,
                eConsole,
            ) {
                // Full-queue device events degrade gracefully: the engine
                // re-checks MMDevice state before acting.
                let _ = self.sender.try_send(event);
            }
            Ok(())
        }

        fn OnDeviceAdded(&self, id: &PCWSTR) -> windows::core::Result<()> {
            if let Some(event) = translate(
                DeviceEvent::Added {
                    endpoint_id: text(id).unwrap_or_default(),
                },
                eRender,
                eConsole,
            ) {
                let _ = self.sender.try_send(event);
            }
            Ok(())
        }

        fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
            if let Some(event) = translate(
                DeviceEvent::Removed {
                    endpoint_id: text(id).unwrap_or_default(),
                },
                eRender,
                eConsole,
            ) {
                let _ = self.sender.try_send(event);
            }
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            id: &PCWSTR,
        ) -> windows::core::Result<()> {
            if let Some(event) = translate(
                DeviceEvent::DefaultChanged {
                    endpoint_id: text(id),
                },
                flow,
                role,
            ) {
                let _ = self.sender.try_send(event);
            }
            Ok(())
        }

        fn OnPropertyValueChanged(
            &self,
            _id: &PCWSTR,
            _key: &PROPERTYKEY,
        ) -> windows::core::Result<()> {
            // Volume/name changes on other apps' devices are noise here; the
            // engine reads properties at open time. Intentionally ignored.
            Ok(())
        }
    }

    /// Register for endpoint notifications. The returned enumerator must be
    /// kept alive for as long as notifications are wanted.
    pub(super) fn register(
        sender: Sender<DeviceEvent>,
    ) -> Result<(IMMNotificationClient, IMMDeviceEnumerator), String> {
        // SAFETY: per-thread COM init; balanced by `unregister`.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(format!("CoInitializeEx failed: {hr}"));
        }
        let initialized_here = hr.is_ok();

        // SAFETY: plain COM class factory call on this thread.
        let enumerator = match unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        } {
            Ok(e) => e,
            Err(e) => {
                if initialized_here {
                    // SAFETY: balanced with the CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
                return Err(format!("create MMDeviceEnumerator: {e}"));
            }
        };

        let notifier: IMMNotificationClient = Notifier { sender }.into();
        // SAFETY: `notifier` stays alive until Unregister below.
        if let Err(e) = unsafe { enumerator.RegisterEndpointNotificationCallback(&notifier) } {
            // SAFETY: balanced with the CoInitializeEx above.
            unsafe { CoUninitialize() };
            return Err(format!("RegisterEndpointNotificationCallback: {e}"));
        }

        Ok((notifier, enumerator))
    }

    pub(super) fn unregister(notifier: &IMMNotificationClient, enumerator: &IMMDeviceEnumerator) {
        // SAFETY: both objects are live here; errors are non-actionable at
        // shutdown (the process is going away).
        unsafe {
            let _ = enumerator.UnregisterEndpointNotificationCallback(notifier);
        }
    }

    pub(super) fn com_uninit() {
        // SAFETY: balanced with the successful CoInitializeEx in `register`.
        unsafe { CoUninitialize() };
    }
}

/// Live registration handle. Keep it alive: dropping it unregisters.
pub struct Notifications {
    #[cfg(windows)]
    inner: Option<(IMMNotificationClient, IMMDeviceEnumerator)>,
    pub active: bool,
}

impl Notifications {
    /// True when notifications are actually registered. When false, the
    /// engine must verify device state at stream-open time instead of
    /// trusting the event stream.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some((notifier, enumerator)) = self.inner.take() {
            imp::unregister(&notifier, &enumerator);
            // Release the COM interfaces BEFORE uninitializing COM (the drop
            // order here must mirror the COM apartment at open time).
            drop(notifier);
            drop(enumerator);
            imp::com_uninit();
        }
    }
}

/// Start listening for device changes. `sender` is the half the playback
/// owner should `select!` on. A failure is reported as an `Unavailable`
/// event (and `is_active() == false`) rather than a panic: playback must
/// survive a machine that cannot notify.
pub fn start(sender: Sender<DeviceEvent>) -> Notifications {
    #[cfg(windows)]
    {
        // `sender` is cloned so the failure path can still report an
        // `Unavailable` event to the engine.
        match imp::register(sender.clone()) {
            Ok(inner) => Notifications {
                inner: Some(inner),
                active: true,
            },
            Err(reason) => {
                tracing::warn!("device notifications unavailable: {reason}");
                let _ = sender.try_send(DeviceEvent::Unavailable { reason });
                Notifications {
                    inner: None,
                    active: false,
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = sender.try_send(DeviceEvent::Unavailable {
            reason: "device notifications are not implemented on this platform".into(),
        });
        Notifications { active: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_change_reports_new_endpoint() {
        let event = DeviceEvent::DefaultChanged {
            endpoint_id: Some("{0.0.0.00000000}".into()),
        };
        assert_eq!(event.endpoint_id(), Some("{0.0.0.00000000}"));
        assert!(!event.makes_endpoint_unusable());
    }

    #[test]
    fn default_change_to_nothing_is_unusable() {
        let event = DeviceEvent::DefaultChanged { endpoint_id: None };
        assert_eq!(event.endpoint_id(), None);
        assert!(event.makes_endpoint_unusable());
    }

    #[test]
    fn removal_makes_endpoint_unusable() {
        let event = DeviceEvent::Removed {
            endpoint_id: "{usb}".into(),
        };
        assert!(event.makes_endpoint_unusable());
    }

    #[test]
    fn state_change_to_inactive_is_unusable_but_active_is_not() {
        assert!(DeviceEvent::StateChanged {
            endpoint_id: "{x}".into(),
            state: DeviceState::Unplugged
        }
        .makes_endpoint_unusable());
        assert!(!DeviceEvent::StateChanged {
            endpoint_id: "{x}".into(),
            state: DeviceState::Active
        }
        .makes_endpoint_unusable());
    }

    #[test]
    fn added_and_unavailable_never_break_playback() {
        assert!(!DeviceEvent::Added {
            endpoint_id: "{x}".into()
        }
        .makes_endpoint_unusable());
        let e = DeviceEvent::Unavailable {
            reason: "test".into(),
        };
        assert!(!e.makes_endpoint_unusable());
        assert_eq!(e.endpoint_id(), None);
    }

    #[cfg(windows)]
    #[test]
    fn translation_filters_non_render_or_non_console_flows() {
        use windows::Win32::Media::Audio::{eCapture, eCommunications, eConsole, eRender};

        let event = || DeviceEvent::Removed {
            endpoint_id: "{x}".into(),
        };
        // The filter lives in `imp::translate`, which requires the Windows
        // constants; assert the render/console pair passes and others do not.
        assert!(imp::translate(event(), eRender, eConsole).is_some());
        assert!(imp::translate(event(), eCapture, eConsole).is_none());
        assert!(imp::translate(event(), eRender, eCommunications).is_none());
    }
}
