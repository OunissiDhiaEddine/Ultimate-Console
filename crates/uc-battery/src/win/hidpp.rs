//! Logitech wireless devices over HID++, through the vendor collection of
//! the receiver (G305 and other LIGHTSPEED / Unifying / Bolt mice and
//! keyboards) or of a headset's own dongle (G535 and friends).
//! Windows doesn't hold that collection exclusively, so this works with or
//! without G HUB running.

use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{DIGCF_DEVICEINTERFACE, DIGCF_PRESENT};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_CAPS, HidD_FreePreparsedData, HidD_GetHidGuid, HidD_GetPreparsedData,
    HidD_GetProductString, HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

use super::DevList;
use crate::hidpp::{self, BATTERY_FEATURES, FEATURE_DEVICE_NAME, LONG_LEN, Reply};
use crate::{Device, Kind, Source, kind_from_name};

/// (usage page, usage) of HID++ long-report collections: receivers and
/// mice use the first, Logitech headset dongles the second.
const LONG_COLLECTIONS: [(u16, u16); 2] = [(0xFF00, 0x0002), (0xFF43, 0x0202)];
/// Per request. A sleeping device answers on the short collection we don't
/// read, so this is how long an absent device costs.
const TIMEOUT: Duration = Duration::from_millis(250);
/// Receiver slots to try. LIGHTSPEED receivers have one device, in slot 1;
/// Unifying and Bolt can pair more.
const RECEIVER_SLOTS: std::ops::RangeInclusive<u8> = 1..=3;
/// Device index for a HID++ device that is plugged in directly.
const DIRECT: u8 = 0xFF;

pub fn scan() -> Vec<Device> {
    // SAFETY: plain FFI call.
    let hid = unsafe { HidD_GetHidGuid() };
    let Some(list) = DevList::new(
        Some(&hid),
        PCWSTR::null(),
        DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (path, _) in list.interfaces(&hid) {
        // Only Logitech, and decide that from the path so nothing else is opened.
        let p = String::from_utf16_lossy(&path).to_ascii_lowercase();
        if !p.contains("vid_046d") {
            continue;
        }
        let Some(dev) = Hid::open(&path) else {
            continue;
        };
        if !LONG_COLLECTIONS.contains(&(dev.caps.UsagePage, dev.caps.Usage))
            || usize::from(dev.caps.OutputReportByteLength) < LONG_LEN
        {
            continue;
        }
        let product = dev.product();
        let slots: Vec<u8> = if product.to_ascii_lowercase().contains("receiver") {
            RECEIVER_SLOTS.collect()
        } else {
            vec![DIRECT]
        };
        tracing::debug!(%product, "logitech hid++ collection");
        for slot in slots {
            if let Some(d) = read_device(&dev, slot, &product) {
                out.push(d);
            }
        }
    }
    out
}

fn read_device(dev: &Hid, slot: u8, product: &str) -> Option<Device> {
    let mut battery = None;
    for (feature, function) in BATTERY_FEATURES {
        let index = match dev.feature_index(slot, feature) {
            Ok(Some(i)) => i,
            Ok(None) => continue,
            Err(()) => {
                tracing::debug!(slot, "logitech: no answer (empty slot or asleep)");
                return None;
            }
        };
        if let Ok(p) = dev.call(slot, index, function, &[])
            && let Some(b) = hidpp::battery(feature, &p)
        {
            battery = Some(b);
            break;
        }
    }
    let Some((level, power)) = battery else {
        tracing::debug!(slot, "logitech: no battery feature");
        return None;
    };
    let (name, kind) = dev
        .name_and_kind(slot)
        .unwrap_or_else(|| (product.to_owned(), Kind::Other));
    let kind = match kind {
        Kind::Other => kind_from_name(&name),
        k => k,
    };
    tracing::debug!(%name, ?level, ?power, slot, "logitech battery");
    Some(Device {
        name,
        kind,
        level: Some(level),
        power,
        source: Source::Logitech,
    })
}

struct Hid {
    handle: HANDLE,
    event: HANDLE,
    caps: HIDP_CAPS,
}

impl Drop for Hid {
    fn drop(&mut self) {
        // SAFETY: handles we opened, closed once.
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.event);
        }
    }
}

impl Hid {
    fn open(path: &[u16]) -> Option<Self> {
        // SAFETY: `path` is NUL-terminated; every out pointer is a local.
        unsafe {
            let handle = CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .ok()?;
            let event = match CreateEventW(None, true, false, PCWSTR::null()) {
                Ok(e) => e,
                Err(_) => {
                    let _ = CloseHandle(handle);
                    return None;
                }
            };
            let mut dev = Self {
                handle,
                event,
                caps: HIDP_CAPS::default(),
            };
            let mut pre = PHIDP_PREPARSED_DATA::default();
            if !HidD_GetPreparsedData(handle, &mut pre) {
                return None;
            }
            let status = HidP_GetCaps(pre, &mut dev.caps);
            let _ = HidD_FreePreparsedData(pre);
            status.is_ok().then_some(dev)
        }
    }

    fn product(&self) -> String {
        let mut buf = [0u16; 128];
        // SAFETY: `buf` is writable and its byte size is passed.
        let ok = unsafe {
            HidD_GetProductString(
                self.handle,
                buf.as_mut_ptr().cast(),
                size_of_val(&buf) as u32,
            )
        };
        if !ok {
            return String::new();
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    /// Wait for an overlapped operation; cancel it on timeout.
    fn finish(
        &self,
        ov: &OVERLAPPED,
        started: Result<(), windows::core::Error>,
        wait: Duration,
    ) -> Option<u32> {
        use windows::Win32::Foundation::ERROR_IO_PENDING;
        let mut n = 0u32;
        match started {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(_) => return None,
        }
        // SAFETY: `ov` and its event stay alive until the operation is
        // complete or cancelled and waited for below.
        unsafe {
            if WaitForSingleObject(self.event, wait.as_millis() as u32) != WAIT_OBJECT_0 {
                let _ = CancelIoEx(self.handle, Some(ov));
                let _ = GetOverlappedResult(self.handle, ov, &mut n, true);
                return None;
            }
            GetOverlappedResult(self.handle, ov, &mut n, false).ok()?;
        }
        Some(n)
    }

    /// Send one request and wait for its answer.
    fn call(&self, slot: u8, index: u8, function: u8, params: &[u8]) -> Result<[u8; 16], ()> {
        let req = hidpp::request(slot, index, function, params);
        let mut out = vec![0u8; usize::from(self.caps.OutputReportByteLength)];
        out[..LONG_LEN].copy_from_slice(&req);
        let mut ov = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        // SAFETY: `out` and `ov` outlive the operation (finish waits for it).
        let started = unsafe { WriteFile(self.handle, Some(&out), None, Some(&mut ov)) };
        self.finish(&ov, started, TIMEOUT).ok_or(())?;

        let deadline = Instant::now() + TIMEOUT;
        let mut buf = vec![0u8; usize::from(self.caps.InputReportByteLength).max(LONG_LEN)];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(());
            }
            let mut ov = OVERLAPPED {
                hEvent: self.event,
                ..Default::default()
            };
            // SAFETY: `buf` and `ov` outlive the operation (finish waits for it).
            let started = unsafe { ReadFile(self.handle, Some(&mut buf), None, Some(&mut ov)) };
            let n = self.finish(&ov, started, left).ok_or(())? as usize;
            match hidpp::reply(&buf[..n.min(buf.len())], &req) {
                Reply::Ok(p) => {
                    let mut r = [0u8; 16];
                    r.copy_from_slice(&p[..16]);
                    return Ok(r);
                }
                Reply::Error(code) => {
                    tracing::debug!(slot, index, function, code, "hid++ error");
                    return Err(());
                }
                Reply::Other => {}
            }
        }
    }

    /// `Ok(None)`: the device answered but doesn't have the feature.
    /// `Err`: no answer at all.
    fn feature_index(&self, slot: u8, feature: u16) -> Result<Option<u8>, ()> {
        let p = self.call(slot, 0, 0, &feature.to_be_bytes())?;
        Ok((p[0] != 0).then_some(p[0]))
    }

    fn name_and_kind(&self, slot: u8) -> Option<(String, Kind)> {
        let index = self.feature_index(slot, FEATURE_DEVICE_NAME).ok()??;
        let len = usize::from(self.call(slot, index, 0, &[]).ok()?[0]);
        let mut name = Vec::with_capacity(len);
        while name.len() < len {
            let part = self.call(slot, index, 1, &[name.len() as u8]).ok()?;
            let take = (len - name.len()).min(16);
            name.extend_from_slice(&part[..take]);
        }
        let name = String::from_utf8_lossy(&name)
            .trim_end_matches('\0')
            .trim()
            .to_owned();
        let kind = self
            .call(slot, index, 2, &[])
            .map(|p| hidpp::kind(p[0]))
            .unwrap_or(Kind::Other);
        (!name.is_empty()).then_some((name, kind))
    }
}
