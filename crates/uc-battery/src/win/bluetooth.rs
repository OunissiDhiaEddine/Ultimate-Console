//! Bluetooth: the battery level Windows keeps on paired devices, and the
//! LE Battery Service read through the Win32 GATT API.

use std::collections::HashMap;

use windows::Win32::Devices::Bluetooth::{
    BLUETOOTH_GATT_FLAG_NONE, BTH_LE_GATT_CHARACTERISTIC, BTH_LE_GATT_CHARACTERISTIC_VALUE,
    BluetoothGATTGetCharacteristicValue, BluetoothGATTGetCharacteristics,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_ContainerId, DEVPKEY_NAME};
use windows::Win32::Foundation::{CloseHandle, DEVPROPKEY, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::{GUID, PCWSTR, w};

use super::{DevList, instance_id, parent, prop_guid, prop_string, prop_u8, prop_u32};
use crate::{
    Device, Kind, Level, Power, Source, clean_bluetooth_name, kind_from_class_of_device,
    kind_from_name,
};

/// `DEVPKEY_Bluetooth_Battery`: battery percentage Windows reads from the
/// device (hands-free AG indicators, LE battery) and shows in Settings.
const BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5),
    pid: 2,
};
/// `DEVPKEY_Bluetooth_ClassOfDevice`.
const CLASS_OF_DEVICE: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x2bd67d8b_8beb_48d5_87e0_6cda3428040a),
    pid: 10,
};
/// `DEVPKEY_Device_IsConnected` (Bluetooth devnodes): false while the
/// device is paired but away, when any battery value would be stale.
const IS_CONNECTED: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83da6326_97a6_4088_9453_a1923f573b29),
    pid: 15,
};
/// Battery Service 0x180F, as the interface class of its GATT service devnode.
const BATTERY_SERVICE: GUID = GUID::from_u128(0x0000180f_0000_1000_8000_00805f9b34fb);
/// Battery Level characteristic.
const BATTERY_LEVEL: u16 = 0x2A19;

/// A Bluetooth gadget's top devnode (`BTHENUM\Dev_…` / `BTHLE\Dev_…`).
struct Gadget {
    name: String,
    kind: Kind,
    connected: bool,
}

fn gadget(devnode: u32) -> Option<Gadget> {
    let name = prop_string(devnode, &DEVPKEY_NAME)?;
    let kind = prop_u32(devnode, &CLASS_OF_DEVICE)
        .and_then(kind_from_class_of_device)
        .unwrap_or_else(|| kind_from_name(&name));
    // Missing means "can't tell": keep it.
    let connected = prop_u8(devnode, &IS_CONNECTED).is_none_or(|b| b != 0);
    Some(Gadget {
        name: clean_bluetooth_name(&name),
        kind,
        connected,
    })
}

fn is_top(id: &str) -> bool {
    let id = id.to_ascii_uppercase();
    id.starts_with("BTHENUM\\DEV_") || id.starts_with("BTHLE\\DEV_")
}

pub fn scan() -> Vec<Device> {
    let mut out = property_scan();
    out.extend(gatt_scan());
    out
}

/// Every Bluetooth devnode with the battery property. Classic headsets set
/// it on their hands-free service node, so the name comes from the gadget
/// node in the same container.
fn property_scan() -> Vec<Device> {
    let mut nodes = Vec::new();
    for e in [w!("BTHENUM"), w!("BTHLE")] {
        if let Some(list) = DevList::new(None, e, DIGCF_PRESENT | DIGCF_ALLCLASSES) {
            nodes.extend(list.devnodes());
        }
    }
    let mut tops: HashMap<GUID, u32> = HashMap::new();
    let mut with_battery = Vec::new();
    for &n in &nodes {
        let container = prop_guid(n, &DEVPKEY_Device_ContainerId);
        if let Some(c) = container
            && is_top(&instance_id(n))
        {
            tops.insert(c, n);
        }
        if let Some(level) = prop_u8(n, &BATTERY) {
            with_battery.push((n, container, level));
        }
    }
    let mut out = Vec::new();
    for (n, container, level) in with_battery {
        let top = container.and_then(|c| tops.get(&c).copied()).unwrap_or(n);
        let Some(g) = gadget(top).or_else(|| gadget(n)) else {
            continue;
        };
        if !g.connected {
            tracing::debug!(name = %g.name, "bluetooth: not connected, battery skipped");
            continue;
        }
        tracing::debug!(name = %g.name, level, id = %instance_id(n), "bluetooth battery property");
        out.push(Device {
            name: g.name,
            kind: g.kind,
            level: Some(Level::Percent(level.min(100))),
            power: Power::Unknown,
            source: Source::Bluetooth,
        });
    }
    out
}

/// Bluetooth LE devices with a Battery Service.
fn gatt_scan() -> Vec<Device> {
    let Some(list) = DevList::new(
        Some(&BATTERY_SERVICE),
        PCWSTR::null(),
        DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (path, service_node) in list.interfaces(&BATTERY_SERVICE) {
        // BTHLEDevice\{0000180f…} → BTHLE\Dev_… (the gadget).
        let Some(g) = parent(service_node).and_then(gadget) else {
            continue;
        };
        if !g.connected {
            tracing::debug!(name = %g.name, "ble: not connected, battery skipped");
            continue;
        }
        match read_level(&path) {
            Some(level) => {
                tracing::debug!(name = %g.name, level, "ble battery service");
                out.push(Device {
                    name: g.name,
                    kind: g.kind,
                    level: Some(Level::Percent(level.min(100))),
                    power: Power::Unknown,
                    source: Source::BluetoothLe,
                });
            }
            None => tracing::debug!(name = %g.name, "ble: battery level not readable"),
        }
    }
    out
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle we opened, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn read_level(path: &[u16]) -> Option<u8> {
    // SAFETY: `path` is NUL-terminated.
    let h = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    let h = Handle(h);
    let mut chars = [BTH_LE_GATT_CHARACTERISTIC::default(); 8];
    let mut count = 0u16;
    // SAFETY: `chars` is a valid buffer; the handle is a GATT service.
    unsafe {
        BluetoothGATTGetCharacteristics(
            h.0,
            None,
            Some(&mut chars),
            &mut count,
            BLUETOOTH_GATT_FLAG_NONE,
        )
    }
    .ok()?;
    let c = chars.iter().take(usize::from(count)).find(|c| {
        // SAFETY: reading the active union member chosen by IsShortUuid.
        c.CharacteristicUuid.IsShortUuid
            && unsafe { c.CharacteristicUuid.Value.ShortUuid } == BATTERY_LEVEL
    })?;
    // Header plus a few data bytes, as u32s for alignment.
    let mut buf = [0u32; 8];
    let value = buf.as_mut_ptr().cast::<BTH_LE_GATT_CHARACTERISTIC_VALUE>();
    // SAFETY: `buf` is 32 bytes, as passed; Windows answers from its cache
    // when it has the value and asks the device otherwise.
    unsafe {
        BluetoothGATTGetCharacteristicValue(
            h.0,
            c,
            size_of_val(&buf) as u32,
            Some(value),
            None,
            BLUETOOTH_GATT_FLAG_NONE,
        )
        .ok()?;
        ((*value).DataSize >= 1).then(|| (*value).Data[0])
    }
}
