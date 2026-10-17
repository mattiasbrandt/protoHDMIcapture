//! The card's USB link on Windows (#36): Windows has no sysfs, so the
//! parent hub tells the link speed and bcdUSB of the device on its port,
//! as USBView asks it. The setup wizard's USB stage shows them and checks
//! the link (wizard.rs).

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
    CM_Get_Parent, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_Address, DEVPKEY_Device_InstanceId, DEVPROP_TYPE_UINT32, DEVPROPTYPE};
use windows::Win32::Devices::Usb::{
    GUID_DEVINTERFACE_USB_HUB, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2, USB_NODE_CONNECTION_INFORMATION_EX,
    USB_NODE_CONNECTION_INFORMATION_EX_V2, USB_PIPE_INFO, UsbFullSpeed, UsbHighSpeed, UsbLowSpeed, UsbSuperSpeed,
};
use windows::Win32::Foundation::{GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;

use super::mf::devnode_string;

/// `USB_PROTOCOLS`: the bit that asks the hub about USB 3 (Usb300).
const USB_300: u32 = 1 << 2;
/// `USB_NODE_CONNECTION_INFORMATION_EX_V2_FLAGS`.
const OPERATING_AT_SUPER_SPEED: u32 = 1 << 0;
const OPERATING_AT_SUPER_SPEED_PLUS: u32 = 1 << 2;
/// Pipes the connection answer has room for: the hub fills in one per
/// open pipe, and the card streams through several. USBView allows 30.
const PIPES: usize = 32;

/// The link of the USB device at devnode `usb`: bcdUSB as sysfs gives it
/// (" 3.20") and the speed in Mbit/s ("5000"). Err: the step that failed.
pub fn link(usb: u32) -> Result<(String, String), String> {
    let port = port_number(usb)?;
    let mut hub = 0u32;
    // Safety: an out-pointer and a live devnode.
    let r = unsafe { CM_Get_Parent(&mut hub, usb, 0) };
    if r != CR_SUCCESS {
        return Err(format!("its hub: CONFIGRET {}", r.0));
    }
    let instance = devnode_string(hub, &DEVPKEY_Device_InstanceId)?.ok_or("its hub has no instance id")?;
    let hub = open(&hub_path(&instance)?)?;
    let (bcd_usb, speed) = connection(&hub, port)?;
    // Without the V2 answer (Windows before 8) the speed alone says it.
    let flags = match connection_v2(&hub, port) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("warning: the card's USB link: {e}; the speed comes from the older answer");
            None
        }
    };
    let mbit = mbit(speed, flags).ok_or_else(|| format!("the hub reports speed {speed}, which this viewer does not know"))?;
    Ok((version(bcd_usb), mbit.to_string()))
}

/// The link speed in Mbit/s as sysfs spells it, from the hub's `Speed`
/// and, where it gave them, the V2 flags (which alone tell SuperSpeed+).
pub fn mbit(speed: u8, flags: Option<u32>) -> Option<&'static str> {
    match flags {
        Some(f) if f & OPERATING_AT_SUPER_SPEED_PLUS != 0 => return Some("10000"),
        Some(f) if f & OPERATING_AT_SUPER_SPEED != 0 => return Some("5000"),
        _ => {}
    }
    match i32::from(speed) {
        s if s == UsbSuperSpeed.0 => Some("5000"),
        s if s == UsbHighSpeed.0 => Some("480"),
        s if s == UsbFullSpeed.0 => Some("12"),
        s if s == UsbLowSpeed.0 => Some("1.5"),
        _ => None,
    }
}

/// bcdUSB as sysfs's `version` prints it (`%2x.%02x`): 0x0320 → " 3.20".
pub fn version(bcd_usb: u16) -> String {
    format!("{:2x}.{:02x}", bcd_usb >> 8, bcd_usb & 0xff)
}

/// The device's port on its hub: `DEVPKEY_Device_Address`, which for a
/// USB device is the hub's connection index.
fn port_number(usb: u32) -> Result<u32, String> {
    let (mut kind, mut port) = (DEVPROPTYPE::default(), 0u32);
    let mut size = size_of::<u32>() as u32;
    // Safety: the buffer is a u32, and `size` says so.
    let r = unsafe { CM_Get_DevNode_PropertyW(usb, &DEVPKEY_Device_Address, &mut kind, Some((&raw mut port).cast()), &mut size, 0) };
    if r != CR_SUCCESS || kind != DEVPROP_TYPE_UINT32 {
        return Err(format!("its port number: CONFIGRET {}", r.0));
    }
    Ok(port)
}

/// The hub's device interface path, NUL-terminated: the first of its hub
/// interfaces.
fn hub_path(instance: &str) -> Result<Vec<u16>, String> {
    let id: Vec<u16> = instance.encode_utf16().chain([0]).collect();
    let mut len = 0u32;
    // Safety: an out-pointer and a NUL-terminated instance id.
    let r = unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, &GUID_DEVINTERFACE_USB_HUB, PCWSTR(id.as_ptr()), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) };
    if r != CR_SUCCESS {
        return Err(format!("its hub's interface: CONFIGRET {}", r.0));
    }
    let mut list = vec![0u16; len as usize];
    // Safety: as above; `list` holds `len` characters.
    let r = unsafe { CM_Get_Device_Interface_ListW(&GUID_DEVINTERFACE_USB_HUB, PCWSTR(id.as_ptr()), &mut list, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) };
    if r != CR_SUCCESS {
        return Err(format!("its hub's interface: CONFIGRET {}", r.0));
    }
    first(&list).ok_or_else(|| "its hub has no hub interface".to_string())
}

/// The first string of a NUL-separated list ending in two NULs, with its
/// NUL; None for an empty list.
fn first(list: &[u16]) -> Option<Vec<u16>> {
    let end = list.iter().position(|&c| c == 0)?;
    (end > 0).then(|| list[..=end].to_vec())
}

/// The hub, opened for its IOCTLs (no read or write of its own).
fn open(path: &[u16]) -> Result<OwnedHandle, String> {
    // Safety: a NUL-terminated path; the handle is owned and closed below.
    let h = unsafe { CreateFileW(PCWSTR(path.as_ptr()), GENERIC_WRITE.0, FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None) }
        .map_err(|e| format!("opening its hub: {e}"))?;
    // Safety: a handle this function opened and nothing else holds.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

/// The port's bcdUSB and `Speed`.
fn connection(hub: &OwnedHandle, port: u32) -> Result<(u16, u8), String> {
    // Room for the pipes that follow the struct; read back unaligned, as
    // the struct is packed.
    let size = size_of::<USB_NODE_CONNECTION_INFORMATION_EX>() + PIPES * size_of::<USB_PIPE_INFO>();
    let mut buf = vec![0u8; size];
    let ask = USB_NODE_CONNECTION_INFORMATION_EX { ConnectionIndex: port, ..Default::default() };
    // Safety: `buf` holds at least the struct.
    unsafe { std::ptr::write_unaligned(buf.as_mut_ptr().cast(), ask) };
    let mut got = 0u32;
    // Safety: the same buffer in and out, its size given; a live handle.
    unsafe {
        DeviceIoControl(
            HANDLE(hub.as_raw_handle()),
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
            Some(buf.as_ptr().cast()),
            size as u32,
            Some(buf.as_mut_ptr().cast()),
            size as u32,
            Some(&mut got),
            None,
        )
    }
    .map_err(|e| format!("asking its hub about port {port}: {e}"))?;
    if (got as usize) < size_of::<USB_NODE_CONNECTION_INFORMATION_EX>() - size_of::<USB_PIPE_INFO>() {
        return Err(format!("its hub answered {got} bytes about port {port}"));
    }
    // Safety: the hub wrote the struct into `buf`.
    let info: USB_NODE_CONNECTION_INFORMATION_EX = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Ok((info.DeviceDescriptor.bcdUSB, info.Speed))
}

/// The port's V2 flags, asked about USB 3.
fn connection_v2(hub: &OwnedHandle, port: u32) -> Result<u32, String> {
    let mut info = USB_NODE_CONNECTION_INFORMATION_EX_V2 {
        ConnectionIndex: port,
        Length: size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as u32,
        ..Default::default()
    };
    info.SupportedUsbProtocols.ul = USB_300;
    let size = size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as u32;
    let p = (&raw mut info).cast();
    // Safety: the struct in and out, its size given; a live handle.
    unsafe { DeviceIoControl(HANDLE(hub.as_raw_handle()), IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2, Some(p), size, Some(p), size, None, None) }
        .map_err(|e| format!("asking its hub about port {port} (V2): {e}"))?;
    // Safety: a u32 union; packed, so read by value.
    Ok(unsafe { info.Flags.ul })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_speed_in_mbits_as_sysfs_spells_it() {
        // The V2 flags win: they alone tell SuperSpeed+.
        assert_eq!(mbit(3, Some(OPERATING_AT_SUPER_SPEED | OPERATING_AT_SUPER_SPEED_PLUS)), Some("10000"));
        assert_eq!(mbit(3, Some(OPERATING_AT_SUPER_SPEED)), Some("5000"));
        // A USB 3 device on a USB 2 port: high speed, no SuperSpeed flag.
        assert_eq!(mbit(2, Some(0)), Some("480"));
        // Without the V2 answer, the speed alone.
        assert_eq!(mbit(3, None), Some("5000"));
        assert_eq!(mbit(2, None), Some("480"));
        assert_eq!(mbit(1, None), Some("12"));
        assert_eq!(mbit(0, None), Some("1.5"));
        assert_eq!(mbit(7, Some(0)), None);
    }

    #[test]
    fn bcd_usb_as_sysfs_prints_it() {
        assert_eq!(version(0x0320), " 3.20");
        assert_eq!(version(0x0210), " 2.10");
        assert_eq!(version(0x0200), " 2.00");
        assert_eq!(version(0x0110), " 1.10");
    }

    #[test]
    fn the_first_of_a_device_interface_list() {
        let list: Vec<u16> = "\\\\?\\usb#root\0\\\\?\\usb#other\0\0".encode_utf16().collect();
        assert_eq!(first(&list), Some("\\\\?\\usb#root\0".encode_utf16().collect()));
        assert_eq!(first(&[0, 0]), None);
        assert_eq!(first(&[]), None);
    }
}
