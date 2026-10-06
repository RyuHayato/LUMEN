//! WASAPI output backend — Phase 2A (shared) + Phase 2B (exclusive).
//!
//! What this implements (honestly scoped):
//! - Default-device open and explicit endpoint-id selection via
//!   IMMDeviceEnumerator; stale/disabled endpoints fail with a clear error
//!   rather than silently falling back to another device.
//! - Shared mode (2A): IAudioClient in `AUDCLNT_SHAREMODE_SHARED` with
//!   EVENTCALLBACK buffering. We hand Windows the *source* PCM format (f32
//!   interleaved); when it differs from the device mix format we set
//!   `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` and say so in diagnostics.
//! - Exclusive mode (2B): real `AUDCLNT_SHAREMODE_EXCLUSIVE` with honest
//!   negotiation through `IAudioClient::IsFormatSupported`. LUMEN keeps the
//!   source sample rate and channel count and only adapts the sample
//!   encoding (f32 → S32 → S16); it never resamples to force exclusivity.
//!   Nothing here is called "bit-perfect": even an f32-at-source-rate
//!   exclusive stream is only "no resampling, no Windows mixer".
//!
//! Threading: all stream methods run on the pipeline thread. COM is
//! initialized per-thread via [`ComGuard`]; no WASAPI object crosses threads.
//! Device notifications live in `device_notifications`.

use std::time::Duration;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE, DEVICE_STATEMASK_ALL, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::audio::output::{
    DeviceState, OutputBackend, OutputDevice, OutputFormat, OutputMode, OutputStream, SampleFormat,
    StreamRequest,
};
use crate::error::AudioError;

/// Shared-mode buffer target: 30 ms. Small enough for responsive pause/seek,
/// large enough to absorb scheduler jitter.
const BUFFER_DURATION_HNS: i64 = 300_000; // 100-ns units

/// COM apartment guard: initializes COM on this thread; uninitializes on
/// drop only if we were the ones who initialized it.
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn new() -> Result<Self, AudioError> {
        // SAFETY: plain per-thread COM init; no invariants to uphold.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            // The apartment already exists in a different mode (typically STA
            // on a Tauri/WebView2 UI thread). MMDevice enumeration and
            // activation both work in STA; we simply do not own the apartment,
            // so we must not uninitialize it.
            return Ok(Self { owned: false });
        }
        if hr.is_err() {
            return Err(AudioError::BackendUnavailable(format!(
                "CoInitializeEx failed: {hr}"
            )));
        }
        Ok(Self {
            owned: hr.is_ok(), // S_OK means we initialized (S_FALSE = already was)
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balanced with the successful CoInitializeEx in `new`.
            unsafe { CoUninitialize() };
        }
    }
}

fn fmt_err(context: &str, e: windows::core::Error) -> AudioError {
    AudioError::BackendUnavailable(format!("{context}: {} ({})", e.message(), e.code()))
}

const WAVE_FORMAT_EXTENSIBLE_TAG: u32 = 0xFFFE;
/// {00000003-0000-0010-8000-00AA00389B71}
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID = windows::core::GUID::from_values(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
);
/// {00000001-0000-0010-8000-00AA00389B71}
const KSDATAFORMAT_SUBTYPE_PCM: windows::core::GUID = windows::core::GUID::from_values(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
);

/// Parse a WAVEFORMATEX(EXTENSIBLE) pointer into our format type.
/// Real devices almost always report the mix format as EXTENSIBLE (tag
/// 0xFFFE) wrapping either IEEE-float or integer PCM — both are handled.
///
/// SAFETY: `ptr` must point to a valid WAVEFORMATEX (e.g. from GetMixFormat);
/// when the tag is EXTENSIBLE it must point to a full WAVEFORMATEXTENSIBLE
/// (guaranteed by cbSize >= 22, which we check before reading).
unsafe fn parse_wave_format(ptr: *const WAVEFORMATEX) -> Result<OutputFormat, AudioError> {
    // WAVEFORMATEX is packed(1) in the windows crate: move it out with an
    // unaligned read, then copy fields to plain locals (references to packed
    // fields are not allowed anywhere, including in format strings).
    let wf = unsafe { std::ptr::read_unaligned(ptr) };
    let tag = u32::from(wf.wFormatTag);
    let sample_rate_hz = wf.nSamplesPerSec;
    let channels = wf.nChannels;
    let bits = wf.wBitsPerSample;
    let cb_size = wf.cbSize;

    let sample_format = if tag == WAVE_FORMAT_IEEE_FLOAT {
        SampleFormat::F32
    } else if tag == WAVE_FORMAT_EXTENSIBLE_TAG && cb_size >= 22 {
        // SAFETY: caller guarantees a full WAVEFORMATEXTENSIBLE (cbSize>=22).
        let wfx = unsafe { std::ptr::read_unaligned(ptr as *const WAVEFORMATEXTENSIBLE) };
        let sub_format = wfx.SubFormat;
        if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            SampleFormat::F32
        } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
            match bits {
                16 => SampleFormat::S16,
                24 => SampleFormat::S24In32,
                32 => SampleFormat::S32,
                other => {
                    return Err(AudioError::UnsupportedFormat(format!(
                        "unsupported PCM mix bit depth: {other}"
                    )))
                }
            }
        } else {
            return Err(AudioError::UnsupportedFormat(format!(
                "unsupported mix sub-format GUID: {sub_format:?}"
            )));
        }
    } else {
        return Err(AudioError::UnsupportedFormat(format!(
            "unsupported mix format tag: {tag}"
        )));
    };

    Ok(OutputFormat {
        sample_rate_hz,
        channels,
        sample_format,
    })
}

/// Plain f32 WAVEFORMATEX — the shared-mode wire format we ask Windows to
/// hand to the audio engine (it converts to the mix format when needed).
fn wave_format_for(format: &OutputFormat) -> WAVEFORMATEX {
    let block_align = u32::from(format.channels) * 4;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: format.channels,
        nSamplesPerSec: format.sample_rate_hz,
        nAvgBytesPerSec: format.sample_rate_hz * block_align,
        nBlockAlign: block_align as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn endpoint_id(device: &IMMDevice) -> Result<String, AudioError> {
    // SAFETY: GetId returns a CoTaskMem-allocated PWSTR; free it after copy.
    unsafe {
        let id = device.GetId().map_err(|e| fmt_err("IMMDevice::GetId", e))?;
        let text = id.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(id.as_ptr() as _));
        Ok(text)
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: standard property-store read with balanced PropVariantClear.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = {
            let inner = &pv.Anonymous.Anonymous;
            if inner.vt == VT_LPWSTR {
                let pwsz = inner.Anonymous.pwszVal;
                if pwsz.is_null() {
                    None
                } else {
                    pwsz.to_string().ok()
                }
            } else {
                None
            }
        };
        let _ = PropVariantClear(&mut pv);
        name
    }
}

/// Bits per sample for a sample encoding.
fn bits_for(sample_format: SampleFormat) -> u16 {
    match sample_format {
        SampleFormat::F32 => 32,
        SampleFormat::S32 => 32,
        SampleFormat::S16 => 16,
        SampleFormat::S24In32 => 24,
    }
}

/// Standard speaker mask for a channel count, or `None` when we cannot
/// describe the layout honestly (LUMEN then refuses exclusive mode for it).
fn channel_mask(channels: u16) -> Option<u32> {
    match channels {
        1 => Some(0x4),   // SPEAKER_FRONT_CENTER
        2 => Some(0x3),   // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        4 => Some(0x33),  // front + back stereo
        6 => Some(0x3F),  // 5.1
        8 => Some(0x63F), // 7.1
        _ => None,
    }
}

/// Sample encodings exclusive negotiation will try, in preference order.
/// The sample *rate* and *channel count* are always the source values:
/// LUMEN never resamples or remaps channels to force exclusive mode.
const EXCLUSIVE_CANDIDATES: [SampleFormat; 3] =
    [SampleFormat::F32, SampleFormat::S32, SampleFormat::S16];

/// WAVEFORMATEXTENSIBLE for exclusive mode, with an explicit channel mask.
fn wave_format_exclusive(format: &OutputFormat) -> WAVEFORMATEXTENSIBLE {
    let bits = bits_for(format.sample_format);
    let block_align = u32::from(format.channels) * u32::from(bits / 8);
    let subtype = match format.sample_format {
        SampleFormat::F32 => KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        _ => KSDATAFORMAT_SUBTYPE_PCM,
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE_TAG as u16,
            nChannels: format.channels,
            nSamplesPerSec: format.sample_rate_hz,
            nAvgBytesPerSec: format.sample_rate_hz * block_align,
            nBlockAlign: block_align as u16,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits,
        },
        dwChannelMask: channel_mask(format.channels).unwrap_or(0),
        SubFormat: subtype,
    }
}

/// Honest conversion note for exclusive mode: source → wire encoding, or
/// "none" when the device takes our format as-is. Explicitly states that no
/// resampling happened (there is none on this path).
fn exclusive_conversion_note(source: &OutputFormat, negotiated: &OutputFormat) -> String {
    if source.sample_format == negotiated.sample_format {
        format!(
            "none (device accepts source f32 {}Hz/{}ch directly)",
            negotiated.sample_rate_hz, negotiated.channels
        )
    } else {
        format!(
            "lumen sample-format conversion (no resampling): f32 {}Hz/{}ch → \
             {} {}Hz/{}ch",
            source.sample_rate_hz,
            source.channels,
            bits_for(negotiated.sample_format),
            negotiated.sample_rate_hz,
            negotiated.channels
        )
    }
}

/// Exclusive format negotiation: pick the first candidate encoding the
/// device reports as supported (`IAudioClient::IsFormatSupported`).
///
/// Pure by design — the probe is injected — so the ordering and the failure
/// message are unit-testable without hardware. On failure we report every
/// format we tried; the caller decides whether to fall back to shared mode.
fn select_exclusive_format(
    source: &OutputFormat,
    probe: &dyn Fn(&OutputFormat) -> bool,
) -> Result<OutputFormat, AudioError> {
    if channel_mask(source.channels).is_none() {
        return Err(AudioError::UnsupportedFormat(format!(
            "exclusive mode: unsupported channel count {}",
            source.channels
        )));
    }

    let mut tried = Vec::new();
    for sample_format in EXCLUSIVE_CANDIDATES {
        let candidate = OutputFormat {
            sample_rate_hz: source.sample_rate_hz,
            channels: source.channels,
            sample_format,
        };
        if probe(&candidate) {
            return Ok(candidate);
        }
        tried.push(format!(
            "{}bit/{}ch@{}Hz",
            bits_for(sample_format),
            candidate.channels,
            candidate.sample_rate_hz
        ));
    }
    Err(AudioError::UnsupportedFormat(format!(
        "device supports none of the exclusive formats LUMEN can provide without \
         resampling: tried {}",
        tried.join(", ")
    )))
}

/// Scale a normalized f32 sample to i32 with clamping.
fn f32_to_s32(s: f32) -> i32 {
    (s * 2_147_483_647.0).clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
}

/// Scale a normalized f32 sample to i16 with clamping.
fn f32_to_s16(s: f32) -> i16 {
    (s * 32_767.0).clamp(-32_768.0, 32_767.0) as i16
}

/// Device default/minimum periods (exclusive-mode buffer sizing).
fn device_period(client: &IAudioClient) -> Result<(i64, i64), AudioError> {
    let mut default_period: i64 = 0;
    let mut min_period: i64 = 0;
    // SAFETY: plain COM call on our thread; both out-params are initialized.
    unsafe {
        client
            .GetDevicePeriod(Some(&mut default_period), Some(&mut min_period))
            .map_err(|e| fmt_err("GetDevicePeriod", e))?
    };
    Ok((default_period, min_period))
}

fn describe(device: &IMMDevice, is_default: bool) -> Result<OutputDevice, AudioError> {
    let id = endpoint_id(device)?;
    let name = friendly_name(device).unwrap_or_else(|| id.clone());
    // SAFETY: plain COM call on our thread.
    let state = unsafe { device.GetState() }
        .map(|s| DeviceState::from_raw(s.0))
        .unwrap_or(DeviceState::Unknown);
    Ok(OutputDevice {
        id,
        name,
        is_default,
        state,
    })
}

fn enumerator() -> Result<IMMDeviceEnumerator, AudioError> {
    // SAFETY: plain COM class factory call.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| fmt_err("create MMDeviceEnumerator", e))
}

pub struct WasapiBackend;

impl Default for WasapiBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WasapiBackend {
    pub fn new() -> Self {
        Self
    }
}

impl OutputBackend for WasapiBackend {
    fn name(&self) -> &'static str {
        "wasapi"
    }

    fn enumerate_devices(&self) -> Result<Vec<OutputDevice>, AudioError> {
        let _com = ComGuard::new()?;
        let enumerator = enumerator()?;
        // SAFETY: standard device enumeration; collection/item lifetimes are
        // scoped to this block.
        unsafe {
            let default_id = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .and_then(|d| d.GetId())
                .map(|id| id.to_string().unwrap_or_default())
                .ok();
            let collection = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE(DEVICE_STATEMASK_ALL))
                .map_err(|e| fmt_err("EnumAudioEndpoints", e))?;
            let count = collection.GetCount().map_err(|e| fmt_err("GetCount", e))?;
            let mut devices = Vec::with_capacity(count as usize);
            for i in 0..count {
                let device = collection.Item(i).map_err(|e| fmt_err("Item", e))?;
                let mut described = describe(&device, false)?;
                described.is_default = default_id.as_deref() == Some(described.id.as_str());
                devices.push(described);
            }
            Ok(devices)
        }
    }

    fn default_device(&self) -> Result<Option<OutputDevice>, AudioError> {
        let _com = ComGuard::new()?;
        let enumerator = enumerator()?;
        // SAFETY: default-endpoint lookup; failure means "no device".
        unsafe {
            match enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
                Ok(device) => describe(&device, true).map(Some),
                Err(_) => Ok(None),
            }
        }
    }

    fn open_stream(
        &mut self,
        device_id: Option<&str>,
        request: &StreamRequest,
    ) -> Result<Box<dyn OutputStream>, AudioError> {
        WasapiStream::open(device_id, request).map(|s| Box::new(s) as Box<dyn OutputStream>)
    }
}

struct WasapiStream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    /// The format actually on the wire (negotiated with the device).
    format: OutputFormat,
    /// The device's mix format (shared mode only; `None` in exclusive).
    mix_format: Option<OutputFormat>,
    conversion: String,
    device_name: String,
    endpoint_id: String,
    mode: OutputMode,
    started: bool,
    // `ComGuard` must drop AFTER the COM-interface fields it protects. Rust
    // drops struct fields in declaration order, so keep it last. Dropping it
    // first would CoUninitialize the apartment and then let
    // `client`/`render`::Release run on a dead apartment (AV at exclusive
    // stream teardown — 0xc0000005).
    _com: ComGuard,
}

impl WasapiStream {
    fn open(device_id: Option<&str>, request: &StreamRequest) -> Result<Self, AudioError> {
        let com = ComGuard::new()?;
        let enumerator = enumerator()?;

        // SAFETY: device activation and client setup follow the documented
        // WASAPI sequence; all COM objects stay on this thread.
        unsafe {
            let device = match device_id {
                Some(id) => {
                    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
                    enumerator
                        .GetDevice(windows::core::PCWSTR(wide.as_ptr()))
                        .map_err(|e| {
                            AudioError::DeviceUnavailable(format!(
                                "endpoint '{id}' not found: {} ({})",
                                e.message(),
                                e.code()
                            ))
                        })?
                }
                None => enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|e| {
                        AudioError::DeviceUnavailable(format!(
                            "no default output device: {} ({})",
                            e.message(),
                            e.code()
                        ))
                    })?,
            };
            let device_name = friendly_name(&device).unwrap_or_else(|| "unknown device".into());
            let device_endpoint_id = endpoint_id(&device)?;

            // Stale/disabled endpoints fail here, deterministically, before
            // any stream state is created. Never silently fall back to a
            // different device the user did not select.
            let device_state = device.GetState().map_err(|e| fmt_err("GetState", e))?;
            if device_state != windows::Win32::Media::Audio::DEVICE_STATE_ACTIVE {
                return Err(AudioError::DeviceUnavailable(format!(
                    "device '{device_name}' is not active (state: {})",
                    DeviceState::from_raw(device_state.0).as_label()
                )));
            }

            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| fmt_err("activate IAudioClient", e))?;

            match request.mode {
                OutputMode::Shared => {
                    Self::open_shared(com, device_name, device_endpoint_id, client, request)
                }
                OutputMode::Exclusive => {
                    Self::open_exclusive(com, device_name, device_endpoint_id, client, request)
                }
            }
        }
    }

    /// Shared mode: negotiate the source format with the Windows mixer,
    /// delegating rate/channel conversion to AUTOCONVERTPCM when needed
    /// (ADR-015). The mix format is probed for honest diagnostics.
    unsafe fn open_shared(
        com: ComGuard,
        device_name: String,
        device_endpoint_id: String,
        client: IAudioClient,
        request: &StreamRequest,
    ) -> Result<Self, AudioError> {
        let mix_ptr = client
            .GetMixFormat()
            .map_err(|e| fmt_err("GetMixFormat", e))?;
        let mix_format = parse_wave_format(mix_ptr)?;
        windows::Win32::System::Com::CoTaskMemFree(Some(mix_ptr as _));

        let format = request.format.unwrap_or(mix_format);
        let needs_conversion = format != mix_format;
        let conversion = if needs_conversion {
            format!(
                "windows-audio-engine (AUTOCONVERTPCM): {}Hz/{}ch f32 → {}Hz/{}ch f32",
                format.sample_rate_hz,
                format.channels,
                mix_format.sample_rate_hz,
                mix_format.channels
            )
        } else {
            "none (source format matches mix format)".to_string()
        };

        let wave_format = wave_format_for(&format);
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if needs_conversion {
            flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
        }

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_DURATION_HNS,
                0,
                &wave_format,
                None,
            )
            .map_err(|e| {
                AudioError::BackendUnavailable(format!(
                    "WASAPI Initialize ({}Hz/{}ch f32 shared): {} ({})",
                    format.sample_rate_hz,
                    format.channels,
                    e.message(),
                    e.code()
                ))
            })?;

        Self::finish_open(
            com,
            client,
            format,
            Some(mix_format),
            conversion,
            device_name,
            device_endpoint_id,
            OutputMode::Shared,
        )
    }

    /// Exclusive mode: honest format negotiation, no Windows mixer.
    ///
    /// The device gets the source rate and channel count verbatim — only the
    /// sample encoding may change (f32 → S32 → S16), and that is reported,
    /// not hidden. If no format works we fail with a meaningful error; the
    /// *caller* decides whether to fall back to shared mode. LUMEN never
    /// resamples to force exclusive mode.
    unsafe fn open_exclusive(
        com: ComGuard,
        device_name: String,
        device_endpoint_id: String,
        client: IAudioClient,
        request: &StreamRequest,
    ) -> Result<Self, AudioError> {
        let Some(source) = request.format else {
            return Err(AudioError::UnsupportedFormat(
                "exclusive mode requires an explicit source format".into(),
            ));
        };

        // `probe` is a pure call-per-candidate wrapper over the COM query;
        // `select_exclusive_format` owns the ordering and failure reporting.
        let probe = |candidate: &OutputFormat| -> bool {
            let wf_ext = wave_format_exclusive(candidate);
            // WAVEFORMATEX is packed(1): take a raw pointer, never a reference.
            let wf = std::ptr::addr_of!(wf_ext.Format);
            let hr = client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, wf, None);
            // S_OK only: S_FALSE means "not supported here, closest match
            // supplied" and must never be treated as supported.
            hr == windows::Win32::Foundation::S_OK
        };
        let format = select_exclusive_format(&source, &probe)?;

        // Exclusive mode: the endpoint period is authoritative. Initialize
        // accepts one of the periods the device supports; the buffer
        // duration must be a whole number of those periods (else
        // AUDCLNT_E_BUFDURATION_PERIOD_NOT_EQUAL / 0x88890013), or the call
        // fails despite IsFormatSupported having agreed on the format. The
        // robust and widely-used choice is one period: duration == period.
        let (default_period, _) = device_period(&client).unwrap_or((0, 0));
        let (periodicity, buffer_hns) = if default_period > 0 {
            (default_period, default_period)
        } else {
            // GetDevicePeriod failed: defer to the device for the period and
            // use the same modest absolute buffer.
            (0, BUFFER_DURATION_HNS)
        };

        let wf_ext = wave_format_exclusive(&format);
        // WAVEFORMATEX is packed(1): raw pointer, never a reference.
        let wf = std::ptr::addr_of!(wf_ext.Format);
        client
            .Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_hns,
                periodicity,
                wf,
                None,
            )
            .map_err(|e| {
                AudioError::BackendUnavailable(format!(
                    "WASAPI Initialize ({}Hz/{}ch {:?} exclusive): {} ({})",
                    format.sample_rate_hz,
                    format.channels,
                    format.sample_format,
                    e.message(),
                    e.code()
                ))
            })?;

        let conversion = exclusive_conversion_note(&source, &format);
        Self::finish_open(
            com,
            client,
            format,
            None,
            conversion,
            device_name,
            device_endpoint_id,
            OutputMode::Exclusive,
        )
    }

    /// Shared tail of `open_shared`/`open_exclusive`: event handle, buffer
    /// size, and the render client.
    #[allow(clippy::too_many_arguments)]
    unsafe fn finish_open(
        com: ComGuard,
        client: IAudioClient,
        format: OutputFormat,
        mix_format: Option<OutputFormat>,
        conversion: String,
        device_name: String,
        endpoint_id: String,
        mode: OutputMode,
    ) -> Result<Self, AudioError> {
        let event =
            CreateEventW(None, false, false, None).map_err(|e| fmt_err("CreateEventW", e))?;
        client
            .SetEventHandle(event)
            .map_err(|e| fmt_err("SetEventHandle", e))?;

        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| fmt_err("GetBufferSize", e))?;
        let render: IAudioRenderClient = client
            .GetService()
            .map_err(|e| fmt_err("GetService<IAudioRenderClient>", e))?;

        Ok(Self {
            _com: com,
            client,
            render,
            event,
            buffer_frames,
            format,
            mix_format,
            conversion,
            device_name,
            endpoint_id,
            mode,
            started: false,
        })
    }
}

impl OutputStream for WasapiStream {
    fn negotiated_format(&self) -> OutputFormat {
        self.format
    }

    fn mode(&self) -> OutputMode {
        self.mode
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    fn mix_format(&self) -> Option<OutputFormat> {
        self.mix_format
    }

    fn conversion_description(&self) -> String {
        self.conversion.clone()
    }

    fn start(&mut self) -> Result<(), AudioError> {
        // SAFETY: plain COM call on our thread.
        unsafe { self.client.Start() }.map_err(|e| fmt_err("IAudioClient::Start", e))?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        // SAFETY: plain COM call on our thread.
        unsafe { self.client.Stop() }.map_err(|e| fmt_err("IAudioClient::Stop", e))?;
        self.started = false;
        Ok(())
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        // SAFETY: waiting on our own event handle.
        let result = unsafe { WaitForSingleObject(self.event, timeout.as_millis() as u32) };
        match result {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            other => Err(AudioError::BackendUnavailable(format!(
                "wait on buffer event failed: {other:?}"
            ))),
        }
    }

    fn writable_frames(&self) -> Result<usize, AudioError> {
        // SAFETY: plain COM call on our thread.
        let padding = unsafe { self.client.GetCurrentPadding() }
            .map_err(|e| fmt_err("GetCurrentPadding", e))?;
        Ok(self.buffer_frames.saturating_sub(padding) as usize)
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError> {
        let channels = self.format.channels as usize;
        let frame_count = frames.len() / channels;
        if frame_count == 0 {
            return Ok(0);
        }
        let writable = self.writable_frames()?;
        let count = frame_count.min(writable);
        if count == 0 {
            return Ok(0);
        }
        // SAFETY: GetBuffer/ReleaseBuffer contract — we write exactly `count`
        // frames of f32 into the returned buffer before releasing.
        unsafe {
            let ptr = self
                .render
                .GetBuffer(count as u32)
                .map_err(|e| fmt_err("GetBuffer", e))?;
            match self.format.sample_format {
                SampleFormat::F32 => {
                    std::ptr::copy_nonoverlapping(
                        frames.as_ptr(),
                        ptr as *mut f32,
                        count * channels,
                    );
                }
                SampleFormat::S32 => {
                    let dst = ptr as *mut i32;
                    let n = count * channels;
                    for (i, &s) in frames.iter().enumerate().take(n) {
                        dst.add(i).write(f32_to_s32(s));
                    }
                }
                SampleFormat::S16 => {
                    let dst = ptr as *mut i16;
                    let n = count * channels;
                    for (i, &s) in frames.iter().enumerate().take(n) {
                        dst.add(i).write(f32_to_s16(s));
                    }
                }
                SampleFormat::S24In32 => {
                    return Err(AudioError::BackendUnavailable(
                        "24-bit-in-32 not supported in exclusive mode".into(),
                    ));
                }
            }
            self.render
                .ReleaseBuffer(count as u32, 0)
                .map_err(|e| fmt_err("ReleaseBuffer", e))?;
        }
        Ok(count)
    }

    fn write_silence(&mut self, frames: usize) -> Result<(), AudioError> {
        let writable = self.writable_frames()?;
        let count = frames.min(writable);
        if count == 0 {
            return Ok(());
        }
        // SAFETY: we release the buffer with the SILENT flag — contents are
        // intentionally not written.
        unsafe {
            let _ptr = self
                .render
                .GetBuffer(count as u32)
                .map_err(|e| fmt_err("GetBuffer(silence)", e))?;
            self.render
                .ReleaseBuffer(count as u32, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                .map_err(|e| fmt_err("ReleaseBuffer(silence)", e))?;
        }
        Ok(())
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: best-effort stop before teardown.
            unsafe {
                let _ = self.client.Stop();
            }
        }
        // SAFETY: closing our own event handle.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}
