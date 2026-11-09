//! The Windows sources. Each one returns what it found and logs (at debug)
//! what it skipped, so `uc-probe battery` can explain a missing device.

mod bluetooth;
mod hidpp;
mod pads;

use std::cell::Cell;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_IDW, CM_Get_Parent, CR_SUCCESS, HDEVINFO,
    SETUP_DI_GET_CLASS_DEVS_FLAGS, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Devices::Properties::DEVPROPTYPE;
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{GUID, PCWSTR};

use crate::Device;

pub fn scan() -> Vec<Device> {
    init_com();
    let mut out = Vec::new();
    out.extend(pads::scan());
    out.extend(bluetooth::scan());
    out.extend(hidpp::scan());
    out
}

thread_local! {
    static COM: Cell<bool> = const { Cell::new(false) };
}

/// Windows.Gaming.Input needs the WinRT apartment on the scanning thread.
/// Done once per thread and never undone: the thread only scans.
fn init_com() {
    COM.with(|c| {
        if !c.get() {
            // SAFETY: plain COM initialisation of the current thread.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_err() {
                tracing::debug!("CoInitializeEx: {hr:?}");
            }
            c.set(true);
        }
    });
}

/// A SetupAPI device list, destroyed on drop.
struct DevList(HDEVINFO);

impl DevList {
    fn new(
        class: Option<&GUID>,
        enumerator: PCWSTR,
        flags: SETUP_DI_GET_CLASS_DEVS_FLAGS,
    ) -> Option<Self> {
        // SAFETY: the GUID and string outlive the call.
        unsafe { SetupDiGetClassDevsW(class.map(|g| g as *const GUID), enumerator, None, flags) }
            .ok()
            .map(Self)
    }

    /// Device interface paths of `class` in this list, with the devnode of each.
    fn interfaces(&self, class: &GUID) -> Vec<(Vec<u16>, u32)> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut data = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: `data` is a valid, sized out struct.
            if unsafe { SetupDiEnumDeviceInterfaces(self.0, None, class, i, &mut data) }.is_err() {
                break;
            }
            let mut needed = 0u32;
            // SAFETY: size query; no buffer.
            let _ = unsafe {
                SetupDiGetDeviceInterfaceDetailW(self.0, &data, None, 0, Some(&mut needed), None)
            };
            if needed == 0 {
                continue;
            }
            // u32 storage keeps the detail struct aligned.
            let mut buf = vec![0u32; (needed as usize).div_ceil(4)];
            let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: `buf` holds `needed` bytes; cbSize is the fixed header
            // size as the API requires.
            let ok = unsafe {
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                SetupDiGetDeviceInterfaceDetailW(
                    self.0,
                    &data,
                    Some(detail),
                    needed,
                    None,
                    Some(&mut info),
                )
            };
            if ok.is_err() {
                continue;
            }
            // SAFETY: DevicePath is a NUL-terminated string inside `buf`.
            let path = unsafe {
                let p = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
                let len = (0..).take_while(|&k| *p.add(k) != 0).count();
                let mut v = std::slice::from_raw_parts(p, len).to_vec();
                v.push(0);
                v
            };
            out.push((path, info.DevInst));
        }
        out
    }

    /// Every devnode in this list.
    fn devnodes(&self) -> Vec<u32> {
        use windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiEnumDeviceInfo;
        let mut out = Vec::new();
        for i in 0.. {
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: `info` is a valid, sized out struct.
            if unsafe { SetupDiEnumDeviceInfo(self.0, i, &mut info) }.is_err() {
                break;
            }
            out.push(info.DevInst);
        }
        out
    }
}

impl Drop for DevList {
    fn drop(&mut self) {
        // SAFETY: the list came from SetupDiGetClassDevsW and is freed once.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Raw bytes and type of a devnode property, if set.
fn prop(devnode: u32, key: &DEVPROPKEY) -> Option<(DEVPROPTYPE, Vec<u8>)> {
    let mut ty = DEVPROPTYPE::default();
    let mut size = 0u32;
    // SAFETY: size query with no buffer.
    unsafe { CM_Get_DevNode_PropertyW(devnode, key, &mut ty, None, &mut size, 0) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: `buf` holds `size` bytes.
    let r = unsafe {
        CM_Get_DevNode_PropertyW(devnode, key, &mut ty, Some(buf.as_mut_ptr()), &mut size, 0)
    };
    (r == CR_SUCCESS).then(|| {
        buf.truncate(size as usize);
        (ty, buf)
    })
}

fn prop_string(devnode: u32, key: &DEVPROPKEY) -> Option<String> {
    let (_, b) = prop(devnode, key)?;
    let wide: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&c| c != 0)
        .collect();
    let s = String::from_utf16_lossy(&wide);
    (!s.trim().is_empty()).then_some(s)
}

fn prop_u8(devnode: u32, key: &DEVPROPKEY) -> Option<u8> {
    prop(devnode, key).and_then(|(_, b)| b.first().copied())
}

fn prop_u32(devnode: u32, key: &DEVPROPKEY) -> Option<u32> {
    let (_, b) = prop(devnode, key)?;
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

fn prop_guid(devnode: u32, key: &DEVPROPKEY) -> Option<GUID> {
    let (_, b) = prop(devnode, key)?;
    let b: [u8; 16] = b.get(..16)?.try_into().ok()?;
    Some(GUID::from_u128(u128::from_be_bytes([
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15],
    ])))
}

fn instance_id(devnode: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a valid writable buffer.
    if unsafe { CM_Get_Device_IDW(devnode, &mut buf, 0) } != CR_SUCCESS {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn parent(devnode: u32) -> Option<u32> {
    let mut p = 0u32;
    // SAFETY: `p` is a valid out pointer.
    (unsafe { CM_Get_Parent(&mut p, devnode, 0) } == CR_SUCCESS).then_some(p)
}
