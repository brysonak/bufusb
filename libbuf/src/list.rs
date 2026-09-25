/*
    bufusb - Tool for flashing USB drives across platforms
    Copyright (C) 2026 Bryson Kelly

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */


use anyhow::Result;
use log::{debug, info};

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
use anyhow::bail;

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub path: String,
    pub size_human: String,
    pub size_bytes: u64,
    pub model: String,
    pub removable: bool,
}

#[cfg(target_os = "linux")]
pub fn list_drives() -> Result<Vec<UsbDevice>> {
    info!("Enumerating block devices");

    let sys_block = Path::new("/sys/block");
    let mut devices = Vec::new();

    for entry in fs::read_dir(sys_block)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        debug!("Inspecting /sys/block/{}", name);

        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("dm-")
            || name.starts_with("sr")
            || name.starts_with("zram")
        {
            continue;
        }

        let dev_path = sys_block.join(&name);

        let read_only = sysfs_flag(&dev_path.join("ro"));
        if read_only {
            continue;
        }

        let size_bytes = sysfs_u64(&dev_path.join("size"))
            .unwrap_or(0)
            .saturating_mul(512);

        if size_bytes == 0 {
            continue;
        }

        // Walk the sysfs device symlink chain upward looking for a "usb" subsystem entry.
        let is_usb = is_usb_device(&dev_path);
        let removable = sysfs_flag(&dev_path.join("removable"));

        let vendor = sysfs_str(&dev_path.join("device").join("vendor")).unwrap_or_default();
        let model_raw = sysfs_str(&dev_path.join("device").join("model")).unwrap_or_default();
        let model = format!("{} {}", vendor.trim(), model_raw.trim())
            .trim()
            .to_string();
        let model = if model.is_empty() {
            "Unknown".to_string()
        } else {
            model
        };

        let path = format!("/dev/{}", name);

        info!(
            "Found: {} | {} | {} | removable={} | usb={}",
            path,
            model,
            human_bytes(size_bytes),
            removable,
            is_usb,
        );

        devices.push(UsbDevice {
            path,
            size_human: human_bytes(size_bytes),
            size_bytes,
            model,
            removable,
        });
    }

    // Sort: USB/removable first, then by path.
    devices.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.path.cmp(&b.path)));

    info!("Total devices found: {}", devices.len());
    Ok(devices)
}

// Walk the sysfs symlink for a block device upward, checking each ancestor's
// "subsystem" symlink for "usb"
#[cfg(target_os = "linux")]
fn is_usb_device(dev_path: &Path) -> bool {
    let device_link = dev_path.join("device");
    let resolved = match fs::canonicalize(&device_link) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let mut current = resolved.as_path();
    loop {
        let subsystem = current.join("subsystem");
        if let Ok(target) = fs::read_link(&subsystem) {
            let target_str = target.to_string_lossy();
            if target_str.contains("usb") {
                return true;
            }
        }
        match current.parent() {
            Some(p) if p != current => current = p,
            _ => break,
        }
    }
    false
}

#[cfg(target_os = "windows")]
pub fn list_drives() -> Result<Vec<UsbDevice>> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
        SPDRP_FRIENDLYNAME, SPDRP_PHYSICAL_DEVICE_OBJECT_NAME, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    };
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_GET_DEVICE_NUMBER,
        STORAGE_DEVICE_NUMBER,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    // Physical disk enumeration
    let disk_guid = windows::core::GUID {
        data1: 0x53f56307,
        data2: 0xb6bf,
        data3: 0x11d0,
        data4: [0x94, 0xf2, 0x00, 0xa0, 0xc9, 0x1e, 0xfb, 0x8b],
    };

    info!("Enumerating physical disks via SetupDI");

    let hdevinfo = unsafe {
        SetupDiGetClassDevsW(
            Some(&disk_guid),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }?;

    let mut devices = Vec::new();
    let mut index = 0u32;

    loop {
        let mut iface_data = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(hdevinfo, None, &disk_guid, index, &mut iface_data)
        };

        if ok.is_err() {
            break;
        }

        index += 1;

        let mut required_size = 0u32;
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdevinfo,
                &iface_data,
                None,
                0,
                Some(&mut required_size),
                None,
            )
        };

        if required_size == 0 {
            continue;
        }

        let mut detail_buf = vec![0u8; required_size as usize];
        let detail = detail_buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe {
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }

        let mut devinfo_data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };

        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdevinfo,
                &iface_data,
                Some(detail),
                required_size,
                None,
                Some(&mut devinfo_data),
            )
        };

        if ok.is_err() {
            continue;
        }

        let device_path_ptr = unsafe { (*detail).DevicePath.as_ptr() };
        let device_path_wide: Vec<u16> = unsafe {
            let mut len = 0;
            while *device_path_ptr.add(len) != 0 {
                len += 1;
            }
            std::slice::from_raw_parts(device_path_ptr, len).to_vec()
        };
        let device_path = OsString::from_wide(&device_path_wide)
            .to_string_lossy()
            .to_string();

        debug!("Found device path: {}", device_path);

        let wide_path: Vec<u16> = device_path_wide
            .iter()
            .copied()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(wide_path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };

        // Wrapped in a File purely so Drop closes it on every exit path below
        let owned = match handle {
            Ok(h) if h != INVALID_HANDLE_VALUE => unsafe {
                std::fs::File::from_raw_handle(h.0 as *mut _)
            },
            Ok(_) => {
                debug!("Could not open {}, skipping", device_path);
                continue;
            }
            Err(e) => {
                debug!("Could not open {} ({}), skipping", device_path, e);
                continue;
            }
        };
        let handle = windows::Win32::Foundation::HANDLE(owned.as_raw_handle() as isize);

        let mut dev_num = STORAGE_DEVICE_NUMBER::default();
        let mut bytes_returned = 0u32;
        let num_ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                None,
                0,
                Some(&mut dev_num as *mut _ as *mut _),
                std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if num_ok.is_err() {
            continue;
        }

        let drive_path = format!("\\\\.\\PhysicalDrive{}", dev_num.DeviceNumber);

        let mut geom = DISK_GEOMETRY_EX::default();
        let geom_ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                None,
                0,
                Some(&mut geom as *mut _ as *mut _),
                std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        let size_bytes = if geom_ok.is_ok() { geom.DiskSize as u64 } else { 0 };

        if size_bytes == 0 {
            continue;
        }

        let (removable, bus_type) = query_device_descriptor(handle);

        // BusTypeVirtual == 14, BusTypeSpaces == 16: skip software-defined storage
        if bus_type == 14 || bus_type == 16 {
            debug!("Skipping virtual/spaces device: {}", drive_path);
            continue;
        }

        let model = query_registry_string(hdevinfo, &mut devinfo_data, SPDRP_FRIENDLYNAME)
            .or_else(|| {
                query_registry_string(
                    hdevinfo,
                    &mut devinfo_data,
                    SPDRP_PHYSICAL_DEVICE_OBJECT_NAME,
                )
            })
            .unwrap_or_else(|| "Unknown".to_string());

        info!(
            "Found: {} | {} | {} | removable={}",
            drive_path,
            model,
            human_bytes(size_bytes),
            removable,
        );

        devices.push(UsbDevice {
            path: drive_path,
            size_human: human_bytes(size_bytes),
            size_bytes,
            model,
            removable,
        });
    }

    unsafe { SetupDiDestroyDeviceInfoList(hdevinfo) }?;

    devices.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.path.cmp(&b.path)));

    info!("Total devices found: {}", devices.len());
    Ok(devices)
}

#[cfg(target_os = "windows")]
fn query_device_descriptor(handle: windows::Win32::Foundation::HANDLE) -> (bool, u32) {
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };

    let mut buf = vec![0u8; 512];
    let mut bytes_returned = 0u32;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    if ok.is_err()
        || bytes_returned < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32
    {
        return (false, 0);
    }

    let desc = unsafe { &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    (desc.RemovableMedia.as_bool(), desc.BusType.0 as u32)
}

#[cfg(target_os = "windows")]
fn query_registry_string(
    hdevinfo: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    devinfo_data: &mut windows::Win32::Devices::DeviceAndDriverInstallation::SP_DEVINFO_DATA,
    // SPDRP_* constants are plain u32... gross
    property: u32,
) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiGetDeviceRegistryPropertyW;

    let mut required = 0u32;
    // First call with null buffer to obtain the required byte count
    let _ = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            hdevinfo,
            devinfo_data,
            property,
            None,
            None,
            Some(&mut required as *mut u32),
        )
    };

    if required == 0 {
        return None;
    }

    let mut buf = vec![0u8; required as usize];
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            hdevinfo,
            devinfo_data,
            property,
            None,
            Some(&mut buf),
            Some(&mut required as *mut u32),
        )
    };

    if ok.is_err() {
        return None;
    }

    let wide: Vec<u16> = buf
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|&c| c != 0)
        .collect();

    Some(OsString::from_wide(&wide).to_string_lossy().trim().to_string())
}

#[cfg(target_os = "macos")]
pub fn list_drives() -> Result<Vec<UsbDevice>> {
    use std::process::Command;

    info!("Enumerating disks via diskutil");

    let output = Command::new("diskutil").args(["list", "-plist"]).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse the AllDisksAndPartitions plist to get disk identifiers, then
    // query each with "diskutil info -plist" for size and media type
    let mut devices = Vec::new();

    // Extract disk identifiers from the simple text list output instead
    let list_out = Command::new("diskutil").arg("list").output()?;
    let list_str = String::from_utf8_lossy(&list_out.stdout);

    for line in list_str.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("/dev/disk") {
            continue;
        }
        let disk_path = trimmed.split_whitespace().next().unwrap_or("").to_string();
        if disk_path.is_empty() {
            continue;
        }

        // "diskutil info -plist /dev/diskN" gives us size, removable, and bus protocol
        let info_out = Command::new("diskutil")
            .args(["info", "-plist", &disk_path])
            .output();

        let info_out = match info_out {
            Ok(o) => o,
            Err(_) => continue,
        };

        let info_str = String::from_utf8_lossy(&info_out.stdout);

        // Parse the key-value pairs we need from the plist XML directly without
        // pulling in a plist crate
        let size_bytes = plist_u64(&info_str, "TotalSize").unwrap_or(0);
        if size_bytes == 0 {
            continue;
        }

        let removable = plist_bool(&info_str, "Ejectable").unwrap_or(false)
            || plist_bool(&info_str, "RemovableMedia").unwrap_or(false);

        let protocol = plist_str(&info_str, "BusProtocol").unwrap_or_default();
        let model = plist_str(&info_str, "MediaName")
            .or_else(|| plist_str(&info_str, "IORegistryEntryName"))
            .unwrap_or_else(|| "Unknown".to_string());

        info!(
            "Found: {} | {} | {} | removable={} | protocol={}",
            disk_path,
            model,
            human_bytes(size_bytes),
            removable,
            protocol,
        );

        devices.push(UsbDevice {
            path: disk_path,
            size_human: human_bytes(size_bytes),
            size_bytes,
            model,
            removable,
        });
    }

    devices.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.path.cmp(&b.path)));

    info!("Total devices found: {}", devices.len());
    Ok(devices)
}

#[cfg(target_os = "macos")]
fn plist_str(xml: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{}</key>", key);
    let pos = xml.find(&needle)?;
    let after = &xml[pos + needle.len()..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

#[cfg(target_os = "macos")]
fn plist_u64(xml: &str, key: &str) -> Option<u64> {
    let needle = format!("<key>{}</key>", key);
    let pos = xml.find(&needle)?;
    let after = &xml[pos + needle.len()..];
    let start = after.find("<integer>")? + "<integer>".len();
    let end = after[start..].find("</integer>")?;
    after[start..start + end].trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn plist_bool(xml: &str, key: &str) -> Option<bool> {
    let needle = format!("<key>{}</key>", key);
    let pos = xml.find(&needle)?;
    let after = &xml[pos + needle.len()..].trim_start().to_owned();
    if after.starts_with("<true/>") {
        Some(true)
    } else if after.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn list_drives() -> Result<Vec<UsbDevice>> {
    bail!("--list is not supported on this platform");
}

#[cfg(target_os = "linux")]
fn sysfs_flag(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .map(|v| v == 1)
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn sysfs_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

#[cfg(target_os = "linux")]
fn sysfs_str(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;

    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

pub fn print_device_table(devices: &[UsbDevice]) {
    if devices.is_empty() {
        println!("No storage devices found.");
        #[cfg(windows)]
        println!("If a drive is plugged in, re-run with --verbose to see why each device was skipped.");
        return;
    }

    let col_path = devices.iter().map(|d| d.path.len()).max().unwrap_or(4).max(6);
    let col_size = devices.iter().map(|d| d.size_human.len()).max().unwrap_or(4).max(8);
    let col_model = devices.iter().map(|d| d.model.len()).max().unwrap_or(5).max(5);

    println!(
        "\n  {:<path$}  {:<size$}  {:<model$}",
        "DEVICE", "SIZE", "MODEL",
        path = col_path, size = col_size, model = col_model,
    );
    println!(
        "  {:-<path$}  {:-<size$}  {:-<model$}",
        "", "", "",
        path = col_path, size = col_size, model = col_model,
    );

    for d in devices {
        println!(
            "  {:<path$}  {:<size$}  {:<model$}",
            d.path, d.size_human, d.model,
            path = col_path, size = col_size, model = col_model,
        );
    }
    println!();
}