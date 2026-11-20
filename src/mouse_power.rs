//! 无线鼠标电量。桌面没有整机电池时，用来填岛上的电量环。
//!
//! 罗技 Lightspeed / Bolt 接收器通过 HID++ 上报电量：长报文接口优先，
//! 统一电量（功能 0x1004）读不到时再退回旧的电量状态（0x1000）。
//! 只打开带鼠标接口的罗技接收器，不碰系统正在独占的鼠标集合。

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub percent: u8,
    pub charging: bool,
}

pub struct Service {
    pub rx: std::sync::mpsc::Receiver<Option<Level>>,
    stop: std::sync::mpsc::Sender<()>,
}

impl Drop for Service {
    fn drop(&mut self) { let _ = self.stop.send(()); }
}

pub fn spawn() -> Service {
    let (tx, rx) = std::sync::mpsc::channel();
    let (stop, stopped) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("daozai-mouse-battery".into()).spawn(move || {
        let mut last = None;
        let mut misses = 0u8;
        loop {
            if stopped.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty) { return; }
            let found = read();
            let next = match found {
                Some(level) => { misses = 0; Some(level) }
                None => {
                    misses = misses.saturating_add(1);
                    if misses >= 2 { None } else { last }
                }
            };
            if next != last {
                if tx.send(next).is_err() { return; }
                last = next;
            }
            // 读不到时加快重试，连续两次失败就清掉电量，断开后不再留着旧百分比。
            let wait = if found.is_none() { Duration::from_secs(5) } else { Duration::from_secs(60) };
            if stopped.recv_timeout(wait) != Err(std::sync::mpsc::RecvTimeoutError::Timeout) { return; }
        }
    }).ok();
    Service { rx, stop }
}

/// 统一电量 / 旧电量状态的参数：百分比、档位、充电状态。
/// 充电状态 1–4 分别是充电中、快充满、已充满、慢充。
pub(crate) fn battery_params(params: &[u8]) -> Option<(u8, bool)> {
    let percent = *params.first()?;
    if percent > 100 { return None; }
    let charging = matches!(params.get(2).copied().unwrap_or(0), 1 | 2 | 3 | 4);
    Some((percent, charging))
}

/// `getFeature` 的应答。功能号 0 且类型、版本都是 0 表示接收器没有这个功能。
pub(crate) fn feature_index(response: &[u8]) -> Option<u8> {
    if response.len() < 7 || response[2] == 0xFF { return None; }
    let index = response[4];
    if index == 0 && response[5] == 0 && response[6] == 0 { None } else { Some(index) }
}

fn is_keyboard_or_headset(name: &str) -> bool {
    let name = name.to_lowercase();
    ["keyboard", "键盘", "headset", "headphone", "耳机"].iter().any(|word| name.contains(word))
}

fn vid_pid(path: &str) -> Option<(u16, u16)> {
    let path = path.to_ascii_lowercase();
    let vid = path.split("vid_").nth(1)?.get(..4)?;
    let pid = path.split("pid_").nth(1)?.get(..4)?;
    Some((u16::from_str_radix(vid, 16).ok()?, u16::from_str_radix(pid, 16).ok()?))
}

#[cfg(windows)]
fn read() -> Option<Level> {
    let ports = hid_ports()?;
    let mice: std::collections::HashSet<(u16, u16)> = ports.iter().filter(|port| port.mouse).map(|port| (port.vid, port.pid)).collect();
    let mut vendors: Vec<_> = ports.into_iter().filter(|port| {
        port.vid == 0x046D && port.usage_page == 0xFF00 && matches!(port.output_len, 7 | 20)
            && (mice.is_empty() || mice.contains(&(port.vid, port.pid)))
    }).collect();
    vendors.sort_by_key(|port| if port.output_len == 20 { 0 } else { 1 });
    for port in vendors {
        let Some(handle) = open_hid(&port.path) else { continue };
        for device in [1u8, 2, 3, 0xFF] {
            if let Some(level) = device_battery(handle, port.output_len, device) {
                close_hid(handle);
                return Some(level);
            }
        }
        close_hid(handle);
    }
    None
}

#[cfg(not(windows))]
fn read() -> Option<Level> { None }

#[cfg(windows)]
struct Port {
    path: Vec<u16>,
    vid: u16,
    pid: u16,
    usage_page: u16,
    output_len: u16,
    mouse: bool,
}

#[cfg(windows)]
fn hid_ports() -> Option<Vec<Port>> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA};
    use windows::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
    use windows::core::{GUID, PCWSTR};

    const MOUSE_CLASS: GUID = GUID::from_u128(0x4d36e96f_e325_11ce_bfc1_08002be10318);
    unsafe {
        let guid = HidD_GetHidGuid();
        let set = SetupDiGetClassDevsW(Some(&guid), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE).ok()?;
        let mut ports = Vec::new();
        let mut index = 0u32;
        loop {
            let mut data = SP_DEVICE_INTERFACE_DATA { cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInterfaces(set, None, &guid, index, &mut data).is_err() { break; }
            index += 1;
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &data, None, 0, Some(&mut needed), None);
            if needed < 8 { continue; }
            let mut buffer = vec![0u8; needed as usize];
            let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = 8;
            let mut info = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiGetDeviceInterfaceDetailW(set, &data, Some(detail), needed, None, Some(&mut info)).is_err() { continue; }
            let path = windows::core::PCWSTR(std::ptr::addr_of!((*detail).DevicePath) as *const u16).to_string().unwrap_or_default();
            let Some((vid, pid)) = vid_pid(&path) else { continue };
            let mouse = info.ClassGuid == MOUSE_CLASS;
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            // 鼠标集合由系统独占；只打开同设备上的厂商接口读取 HID++。
            let vendor = vid == 0x046D && !mouse && !path.to_ascii_lowercase().contains("\\kbd");
            let (usage_page, output_len) = if vendor { caps_of(&wide).unwrap_or((0, 0)) } else { (0, 0) };
            ports.push(Port { path: wide, vid, pid, usage_page, output_len, mouse });
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        Some(ports)
    }
}

#[cfg(windows)]
fn caps_of(path: &[u16]) -> Option<(u16, u16)> {
    use windows::Win32::Devices::HumanInterfaceDevice::{HidD_FreePreparsedData, HidD_GetPreparsedData, HidP_GetCaps, HIDP_CAPS, HIDP_STATUS_SUCCESS, PHIDP_PREPARSED_DATA};
    let handle = open_hid(path)?;
    unsafe {
        let mut preparsed = PHIDP_PREPARSED_DATA::default();
        if HidD_GetPreparsedData(handle, &mut preparsed).0 == 0 { close_hid(handle); return None; }
        let mut caps = HIDP_CAPS::default();
        let status = HidP_GetCaps(preparsed, &mut caps);
        let _ = HidD_FreePreparsedData(preparsed);
        close_hid(handle);
        (status == HIDP_STATUS_SUCCESS).then_some((caps.UsagePage, caps.OutputReportByteLength))
    }
}

fn open_hid(path: &[u16]) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(path.as_ptr()),
            0xC0000000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            HANDLE::default(),
        )
    }.ok()?;
    if handle.is_invalid() { None } else { Some(handle) }
}

#[cfg(windows)]
fn close_hid(handle: windows::Win32::Foundation::HANDLE) {
    unsafe { let _ = windows::Win32::Foundation::CloseHandle(handle); }
}

#[cfg(windows)]
fn device_battery(handle: windows::Win32::Foundation::HANDLE, output_len: u16, device: u8) -> Option<Level> {
    let (report, len) = if output_len >= 20 { (0x11u8, 20usize) } else { (0x10, 7) };
    let version = hidpp(handle, report, len, device, 0, 1, &[0, 0])?;
    if version.get(2).copied() != Some(0) { return None; }
    let level = if let Some(index) = feature_of(handle, report, len, device, 0x1004) {
        hidpp(handle, report, len, device, index, 1, &[0]).and_then(|response| battery_params(&response[4..]))
    } else { None }.or_else(|| {
        let index = feature_of(handle, report, len, device, 0x1000)?;
        hidpp(handle, report, len, device, index, 0, &[0]).and_then(|response| battery_params(&response[4..]))
    })?;
    if device_name(handle, report, len, device).is_some_and(|name| is_keyboard_or_headset(&name)) { return None; }
    Some(Level { percent: level.0, charging: level.1 })
}

#[cfg(windows)]
fn feature_of(handle: windows::Win32::Foundation::HANDLE, report: u8, len: usize, device: u8, id: u16) -> Option<u8> {
    let response = hidpp(handle, report, len, device, 0, 0, &[(id >> 8) as u8, id as u8])?;
    feature_index(&response)
}

#[cfg(windows)]
fn device_name(handle: windows::Win32::Foundation::HANDLE, report: u8, len: usize, device: u8) -> Option<String> {
    let index = feature_of(handle, report, len, device, 0x0005)?;
    let count = hidpp(handle, report, len, device, index, 0, &[0])?.get(4).copied().unwrap_or(0) as usize;
    if count == 0 || count > 64 { return None; }
    let mut bytes = Vec::new();
    while bytes.len() < count {
        let chunk = hidpp(handle, report, len, device, index, 1, &[bytes.len() as u8])?;
        let take = (count - bytes.len()).min(len.saturating_sub(4));
        if take == 0 { break; }
        bytes.extend_from_slice(&chunk[4..4 + take]);
    }
    bytes.truncate(count);
    Some(String::from_utf8_lossy(&bytes).trim_matches('\0').trim().to_string())
}

#[cfg(windows)]
fn hidpp(handle: windows::Win32::Foundation::HANDLE, report: u8, len: usize, device: u8, feature: u8, function: u8, params: &[u8]) -> Option<Vec<u8>> {
    const SW: u8 = 0xA;
    let mut output = vec![0u8; len];
    output[0] = report;
    output[1] = device;
    output[2] = feature;
    output[3] = (function << 4) | SW;
    output[4..4 + params.len().min(len.saturating_sub(4))].copy_from_slice(&params[..params.len().min(len.saturating_sub(4))]);
    write_report(handle, &output)?;
    let expect = (function << 4) | SW;
    let started = Instant::now();
    for _ in 0..6 {
        let input = read_report(handle, len, 250)?;
        if input.first() == Some(&report) && input.get(1) == Some(&device) && input.get(2) == Some(&feature) && input.get(3) == Some(&expect) {
            return Some(input);
        }
        if started.elapsed() > Duration::from_millis(700) { break; }
    }
    None
}

#[cfg(windows)]
fn write_report(handle: windows::Win32::Foundation::HANDLE, data: &[u8]) -> Option<()> {
    transfer(handle, Some(data), 0, 400).map(|_| ())
}

#[cfg(windows)]
fn read_report(handle: windows::Win32::Foundation::HANDLE, len: usize, timeout_ms: u32) -> Option<Vec<u8>> {
    transfer(handle, None, len, timeout_ms)
}

#[cfg(windows)]
fn overlapped_write(handle: windows::Win32::Foundation::HANDLE, data: &[u8], timeout_ms: u32) -> bool {
    use windows::Win32::Foundation::{ERROR_IO_PENDING, WAIT_OBJECT_0};
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    unsafe {
        let Ok(event) = CreateEventW(None, true, false, windows::core::PCWSTR::null()) else { return false };
        let mut overlapped = OVERLAPPED { hEvent: event, ..Default::default() };
        let mut written = 0u32;
        let pending = match WriteFile(handle, Some(data), Some(&mut written), Some(&mut overlapped)) {
            Ok(()) => false,
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => true,
            Err(_) => { let _ = windows::Win32::Foundation::CloseHandle(event); return false; }
        };
        if pending && WaitForSingleObject(event, timeout_ms) != WAIT_OBJECT_0 {
            let _ = CancelIoEx(handle, Some(std::ptr::addr_of!(overlapped)));
            let _ = WaitForSingleObject(event, 50);
            let _ = windows::Win32::Foundation::CloseHandle(event);
            return false;
        }
        if pending { let _ = GetOverlappedResult(handle, &overlapped, &mut written, false); }
        let _ = windows::Win32::Foundation::CloseHandle(event);
        true
    }
}

#[cfg(windows)]
fn overlapped_read(handle: windows::Win32::Foundation::HANDLE, len: usize, timeout_ms: u32) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{ERROR_IO_PENDING, WAIT_OBJECT_0};
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    unsafe {
        let event = CreateEventW(None, true, false, windows::core::PCWSTR::null()).ok()?;
        let mut overlapped = OVERLAPPED { hEvent: event, ..Default::default() };
        let mut buffer = vec![0u8; len];
        let mut read = 0u32;
        let pending = match ReadFile(handle, Some(&mut buffer), Some(&mut read), Some(&mut overlapped)) {
            Ok(()) => false,
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => true,
            Err(_) => { let _ = windows::Win32::Foundation::CloseHandle(event); return None; }
        };
        if pending && WaitForSingleObject(event, timeout_ms) != WAIT_OBJECT_0 {
            let _ = CancelIoEx(handle, Some(std::ptr::addr_of!(overlapped)));
            let _ = WaitForSingleObject(event, 50);
            let _ = windows::Win32::Foundation::CloseHandle(event);
            return None;
        }
        if pending && GetOverlappedResult(handle, &overlapped, &mut read, false).is_err() {
            let _ = windows::Win32::Foundation::CloseHandle(event);
            return None;
        }
        let _ = windows::Win32::Foundation::CloseHandle(event);
        Some(buffer)
    }
}

#[cfg(windows)]
fn transfer(handle: windows::Win32::Foundation::HANDLE, output: Option<&[u8]>, input_len: usize, timeout_ms: u32) -> Option<Vec<u8>> {
    if let Some(output) = output {
        if !overlapped_write(handle, output, timeout_ms) { return None; }
        if input_len == 0 { return Some(Vec::new()); }
    }
    overlapped_read(handle, input_len, timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pro_x_superlight_status_is_a_percentage() {
        assert_eq!(battery_params(&[0x4e, 0x08, 0x00, 0x00]), Some((78, false)));
        assert_eq!(battery_params(&[50, 0, 1]), Some((50, true)));
        assert_eq!(battery_params(&[100, 0, 3]), Some((100, true)));
        assert_eq!(battery_params(&[101, 0, 0]), None);
    }

    #[test]
    fn missing_hidpp_feature_is_not_a_real_index() {
        assert_eq!(feature_index(&[0x11, 0x01, 0x00, 0x0A, 0x06, 0x00, 0x05]), Some(0x06));
        assert_eq!(feature_index(&[0x11, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00]), None);
        assert_eq!(feature_index(&[0x11, 0x01, 0xFF, 0x0A, 0x00, 0x08, 0x00]), None);
    }

    #[test]
    fn keyboard_names_are_not_used_as_the_mouse() {
        assert!(is_keyboard_or_headset("G915 Keyboard"));
        assert!(is_keyboard_or_headset("耳机"));
        assert!(!is_keyboard_or_headset("PRO X SUPERLIGHT 2c"));
    }

    #[test]
    fn receiver_path_keeps_the_vid_and_pid() {
        assert_eq!(vid_pid(r"\\?\hid#vid_046d&pid_c54d&mi_02&col02#a&1#{guid}"), Some((0x046D, 0xC54D)));
    }

    #[test]
    fn connected_logitech_mouse_reports_a_percentage() {
        if std::env::var_os("DAOZAI_READ_MOUSE").is_none() { return; }
        let level = read().expect("应读到已连接的罗技鼠标电量");
        assert!(level.percent <= 100);
        eprintln!("mouse {}% charging={}", level.percent, level.charging);
    }
}
