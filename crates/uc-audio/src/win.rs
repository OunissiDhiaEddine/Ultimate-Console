//! Windows Core Audio implementation.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::path::Path;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, RPC_E_CHANGED_MODE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, ERole, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR};

use crate::{AppSession, AudioControl, AudioDevice, Error, Result, check_level, merge_sessions};

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: what `GetDefaultAudioEndpoint`
/// returns when there is no output device at all.
const E_NOTFOUND: HRESULT = HRESULT(0x8007_0490_u32 as i32);

/// Attaches the name of the failing call to a `windows` error.
trait Context<T> {
    fn ctx(self, context: &'static str) -> Result<T>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T> {
        self.map_err(|e| Error::Os {
            context,
            code: e.code().0 as u32,
            message: e.message(),
        })
    }
}

/// Initialises COM on the current thread and undoes it on drop. If the thread
/// already uses COM in another apartment (an STA UI thread), that apartment
/// is used as-is and nothing is undone.
struct ComGuard {
    uninit: bool,
    // COM initialisation is per thread.
    _not_send: PhantomData<*const ()>,
}

impl ComGuard {
    fn new() -> Result<Self> {
        // SAFETY: plain COM initialisation with no reserved pointer.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self {
                uninit: false,
                _not_send: PhantomData,
            });
        }
        hr.ok().ctx("CoInitializeEx")?;
        Ok(Self {
            uninit: true,
            _not_send: PhantomData,
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: balances the successful CoInitializeEx on this same
            // thread (the guard is !Send).
            unsafe { CoUninitialize() };
        }
    }
}

/// Core Audio access for the calling thread.
///
/// Create it on the thread that uses it (it is not `Send`). It initialises COM
/// (multithreaded) if the thread hasn't already. Every call looks up the
/// current default device again, so a device change made elsewhere is picked
/// up without recreating it.
pub struct CoreAudio {
    enumerator: IMMDeviceEnumerator,
    // Declared last so COM is uninitialised after the enumerator is released.
    _com: ComGuard,
}

impl CoreAudio {
    pub fn new() -> Result<Self> {
        let com = ComGuard::new()?;
        // SAFETY: COM is initialised on this thread by `com`.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .ctx("CoCreateInstance(MMDeviceEnumerator)")?;
        Ok(Self {
            enumerator,
            _com: com,
        })
    }

    fn default_device(&self) -> Result<IMMDevice> {
        // SAFETY: valid enumerator, constant enum arguments.
        match unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) } {
            Err(e) if e.code() == E_NOTFOUND => Err(Error::NoDefaultDevice),
            r => r.ctx("GetDefaultAudioEndpoint"),
        }
    }

    fn endpoint_volume(&self) -> Result<IAudioEndpointVolume> {
        let device = self.default_device()?;
        // SAFETY: activating a documented interface with no parameters.
        unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
            .ctx("IMMDevice::Activate(IAudioEndpointVolume)")
    }

    /// Every live session on the default output, with its volume control.
    fn raw_sessions(&self) -> Result<Vec<(AppSession, ISimpleAudioVolume)>> {
        let device = self.default_device()?;
        // SAFETY: COM calls on valid interfaces; out-values are owned by the
        // returned wrappers.
        unsafe {
            let manager = device
                .Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)
                .ctx("IMMDevice::Activate(IAudioSessionManager2)")?;
            let sessions = manager.GetSessionEnumerator().ctx("GetSessionEnumerator")?;
            let count = sessions
                .GetCount()
                .ctx("IAudioSessionEnumerator::GetCount")?;
            let mut out = Vec::new();
            for i in 0..count {
                let control = sessions
                    .GetSession(i)
                    .ctx("IAudioSessionEnumerator::GetSession")?;
                if control.GetState().ctx("GetState")? == AudioSessionStateExpired {
                    continue;
                }
                let control: IAudioSessionControl2 = control
                    .cast()
                    .ctx("QueryInterface(IAudioSessionControl2)")?;
                let volume: ISimpleAudioVolume =
                    control.cast().ctx("QueryInterface(ISimpleAudioVolume)")?;
                let system = control.IsSystemSoundsSession() == S_OK;
                let pid = if system {
                    0
                } else {
                    // Fails for sessions that span several processes; skip those.
                    match control.GetProcessId() {
                        Ok(pid) => pid,
                        Err(_) => continue,
                    }
                };
                let name = if system {
                    "System Sounds".to_owned()
                } else {
                    process_name(pid).unwrap_or_else(|| {
                        control
                            .GetDisplayName()
                            .map(take_co_string)
                            .unwrap_or_default()
                    })
                };
                let session = AppSession {
                    pid,
                    name,
                    volume: volume
                        .GetMasterVolume()
                        .ctx("ISimpleAudioVolume::GetMasterVolume")?,
                    muted: volume
                        .GetMute()
                        .ctx("ISimpleAudioVolume::GetMute")?
                        .as_bool(),
                };
                out.push((session, volume));
            }
            Ok(out)
        }
    }

    fn for_app(&self, pid: u32, f: impl Fn(&ISimpleAudioVolume) -> Result<()>) -> Result<()> {
        let mut found = false;
        for (s, volume) in self.raw_sessions()? {
            if s.pid == pid {
                f(&volume)?;
                found = true;
            }
        }
        if found {
            Ok(())
        } else {
            Err(Error::NoSession(pid))
        }
    }

    fn describe(&self, device: &IMMDevice, default_id: Option<&str>) -> Result<AudioDevice> {
        // SAFETY: valid device; GetId's string is CoTaskMemAlloc'd and freed
        // by `take_co_string`.
        let id = take_co_string(unsafe { device.GetId() }.ctx("IMMDevice::GetId")?);
        let name = friendly_name(device).unwrap_or_else(|| id.clone());
        Ok(AudioDevice {
            is_default: default_id == Some(id.as_str()),
            id,
            name,
        })
    }

    fn default_id(&self) -> Result<Option<String>> {
        match self.default_device() {
            // SAFETY: as in `describe`.
            Ok(d) => Ok(Some(take_co_string(
                unsafe { d.GetId() }.ctx("IMMDevice::GetId")?,
            ))),
            Err(Error::NoDefaultDevice) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl AudioControl for CoreAudio {
    fn outputs(&self) -> Result<Vec<AudioDevice>> {
        let default_id = self.default_id()?;
        // SAFETY: valid enumerator; items are owned wrappers.
        unsafe {
            let collection = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .ctx("EnumAudioEndpoints")?;
            let count = collection.GetCount().ctx("IMMDeviceCollection::GetCount")?;
            (0..count)
                .map(|i| {
                    let device = collection.Item(i).ctx("IMMDeviceCollection::Item")?;
                    self.describe(&device, default_id.as_deref())
                })
                .collect()
        }
    }

    fn default_output(&self) -> Result<Option<AudioDevice>> {
        match self.default_device() {
            Ok(d) => {
                let mut dev = self.describe(&d, None)?;
                dev.is_default = true;
                Ok(Some(dev))
            }
            Err(Error::NoDefaultDevice) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_default_output(&self, id: &str) -> Result<()> {
        // Make sure the id names an active output (not a microphone, not an
        // unplugged device) before handing it to the undocumented interface,
        // so a typo gives a clear error.
        if !self.outputs()?.iter().any(|d| d.id == id) {
            return Err(Error::DeviceNotFound(id.to_owned()));
        }
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        // `wide` is NUL-terminated and outlives every use of this pointer.
        let pcwstr = PCWSTR(wide.as_ptr());
        let policy = PolicyConfig::new()?;
        for role in [eConsole, eMultimedia, eCommunications] {
            policy.set_default_endpoint(pcwstr, role)?;
        }
        tracing::info!(device = id, "default audio output changed");
        Ok(())
    }

    fn volume(&self) -> Result<f32> {
        // SAFETY: valid interface.
        unsafe { self.endpoint_volume()?.GetMasterVolumeLevelScalar() }
            .ctx("GetMasterVolumeLevelScalar")
    }

    fn set_volume(&self, level: f32) -> Result<()> {
        let level = check_level(level)?;
        // SAFETY: valid interface; a null event context is allowed.
        unsafe {
            self.endpoint_volume()?
                .SetMasterVolumeLevelScalar(level, std::ptr::null())
        }
        .ctx("SetMasterVolumeLevelScalar")?;
        tracing::debug!(level, "master volume set");
        Ok(())
    }

    fn muted(&self) -> Result<bool> {
        // SAFETY: valid interface.
        Ok(unsafe { self.endpoint_volume()?.GetMute() }
            .ctx("IAudioEndpointVolume::GetMute")?
            .as_bool())
    }

    fn set_muted(&self, muted: bool) -> Result<()> {
        // SAFETY: valid interface; a null event context is allowed.
        unsafe { self.endpoint_volume()?.SetMute(muted, std::ptr::null()) }
            .ctx("IAudioEndpointVolume::SetMute")?;
        tracing::debug!(muted, "master mute set");
        Ok(())
    }

    fn sessions(&self) -> Result<Vec<AppSession>> {
        Ok(merge_sessions(
            self.raw_sessions()?.into_iter().map(|(s, _)| s),
        ))
    }

    fn set_app_volume(&self, pid: u32, level: f32) -> Result<()> {
        let level = check_level(level)?;
        self.for_app(pid, |v| {
            // SAFETY: valid interface; a null event context is allowed.
            unsafe { v.SetMasterVolume(level, std::ptr::null()) }
                .ctx("ISimpleAudioVolume::SetMasterVolume")
        })?;
        tracing::debug!(pid, level, "app volume set");
        Ok(())
    }

    fn set_app_muted(&self, pid: u32, muted: bool) -> Result<()> {
        self.for_app(pid, |v| {
            // SAFETY: valid interface; a null event context is allowed.
            unsafe { v.SetMute(muted, std::ptr::null()) }.ctx("ISimpleAudioVolume::SetMute")
        })?;
        tracing::debug!(pid, muted, "app mute set");
        Ok(())
    }
}

/// Copies a `CoTaskMemAlloc`'d wide string and frees it.
fn take_co_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is a valid NUL-terminated string from a COM call that
    // transfers ownership to us; it is freed exactly once here.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: valid device; the PROPVARIANT is cleared after use and the
    // string from PropVariantToStringAlloc is freed by `take_co_string`.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = PropVariantToStringAlloc(&value).ok().map(take_co_string);
        let _ = PropVariantClear(&mut value);
        name.filter(|n| !n.is_empty())
    }
}

/// `game.exe` for `pid`, or `None` if the process can't be opened.
fn process_name(pid: u32) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf`/`len` describe a valid buffer; the handle is closed.
    let ok = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let r = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        r.is_ok()
    };
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// The undocumented `IPolicyConfig` (Windows 7 and later, including 11),
/// the only way to change the default endpoint. Every audio switcher uses it.
/// Only `SetDefaultEndpoint` is needed, so it is called through the raw
/// vtable instead of declaring the whole interface.
struct PolicyConfig(IUnknown);

/// `CPolicyConfigClient`.
const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
/// `IPolicyConfig` (Windows 7+; Vista used a different IID).
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// Layout of the `IPolicyConfig` vtable up to `SetDefaultEndpoint`.
#[repr(C)]
struct PolicyConfigVtbl {
    _iunknown: [usize; 3],
    // GetMixFormat, GetDeviceFormat, ResetDeviceFormat, SetDeviceFormat,
    // GetProcessingPeriod, SetProcessingPeriod, GetShareMode, SetShareMode,
    // GetPropertyValue, SetPropertyValue.
    _unused: [usize; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
}

impl PolicyConfig {
    fn new() -> Result<Self> {
        // SAFETY: COM is initialised (we are inside a CoreAudio call). The
        // queried pointer is owned by the IUnknown wrapper, which releases it.
        unsafe {
            let unknown: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)
                .ctx("CoCreateInstance(CPolicyConfigClient)")?;
            let mut raw = std::ptr::null_mut();
            unknown
                .query(&IID_POLICY_CONFIG, &mut raw)
                .ok()
                .ctx("QueryInterface(IPolicyConfig)")?;
            Ok(Self(IUnknown::from_raw(raw)))
        }
    }

    fn set_default_endpoint(&self, id: PCWSTR, role: ERole) -> Result<()> {
        // SAFETY: `self.0` holds an IPolicyConfig pointer (from QueryInterface
        // with its IID), whose vtable starts with the layout above; `id` is a
        // valid NUL-terminated string for the duration of the call.
        unsafe {
            let this = self.0.as_raw();
            let vtbl = *(this as *const *const PolicyConfigVtbl);
            ((*vtbl).set_default_endpoint)(this, id, role)
        }
        .ok()
        .ctx("IPolicyConfig::SetDefaultEndpoint")
    }
}
