//! Process loopback: everything the machine plays except one process tree, on every output.
//!
//! This is what the desktop leg records by default. Endpoint loopback — the other way — reads one
//! render device's mix, so it misses audio an application sends anywhere but the default output,
//! and it hears Clipper itself: the replay-saved sound, a clip previewed in the grid. Process
//! loopback in exclude mode asks the audio engine for the mix of every process except Clipper's
//! tree, whatever device each one plays to, and the engine does the mixing.
//!
//! Measured against endpoint loopback before it went in (DESIGN.md, "What recording every app
//! corrected"): same CPU, timestamps a constant 6.3 ms apart, no drift over five minutes, packets
//! through silence without a keep-alive stream, and a tone pinned to a second output captured where
//! endpoint loopback got nothing.
//!
//! Windows 10 2004 and later. Anywhere it cannot be activated, the desktop leg falls back to endpoint
//! loopback on the default output.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{implement, Interface, Ref, Result, HRESULT};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl, BLOB};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

/// The process whose tree is left out. Zero until the daemon names Clipper's main process, which is
/// the parent it was spawned by; until then, this process alone.
static EXCLUDED: AtomicU32 = AtomicU32::new(0);

/// Leaves `pid` and everything under it out of the desktop leg. Clipper's main process, so its
/// renderer — the replay-saved sound, a clip playing in the grid — and this recorder are all left
/// out: Chromium plays audio from a utility process of its own, and a tree is the only thing that
/// catches it.
pub fn exclude_tree(pid: u32) {
    EXCLUDED.store(pid, Ordering::Relaxed);
}

fn excluded() -> u32 {
    match EXCLUDED.load(Ordering::Relaxed) {
        0 => unsafe { GetCurrentProcessId() },
        pid => pid,
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Done(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        unsafe { SetEvent(self.0) }
    }
}
impl IAgileObject_Impl for Done_Impl {}

/// An uninitialised client on the process-loopback virtual device, leaving out the excluded tree.
///
/// Activation is asynchronous and can take a second while a device is starting, which is one of the
/// reasons the desktop leg lives on a thread of its own. It has no mix format to ask for — the
/// caller initialises it with the format it wants and the engine converts to it.
pub fn activate() -> Result<IAudioClient> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: excluded(),
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // Never dropped: PROPVARIANT's Drop is PropVariantClear, which would free `params` — a stack
        // value it does not own. Doing so corrupted the heap the first time this ran.
        let prop = ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &params as *const _ as *mut u8,
                        },
                    },
                }),
            },
        });
        let event = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = Done(event).into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop),
            &handler,
        );
        let waited = operation.is_ok().then(|| WaitForSingleObject(event, 5000));
        // Left open on a timeout: the handler may still fire, and signalling a closed handle could
        // signal whatever reused its value. One event is a cheap thing to leak.
        if waited != Some(windows::Win32::Foundation::WAIT_TIMEOUT) {
            let _ = CloseHandle(event);
        }
        let operation = operation?;
        if waited != Some(WAIT_OBJECT_0) {
            return Err(windows::core::Error::new(
                E_FAIL,
                "process loopback did not activate within five seconds",
            ));
        }
        let mut hr = HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok()?;
        unknown
            .ok_or_else(|| windows::core::Error::new(E_FAIL, "process loopback activated to nothing"))?
            .cast()
    }
}
