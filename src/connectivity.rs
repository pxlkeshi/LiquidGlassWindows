//! Wi-Fi / Ethernet / Bluetooth status from Windows, plus turning the radios on and off.
//!
//! A background thread reads everything and keeps it up to date: Windows tells us right
//! away when the network or a radio changes, and we also re-check every few seconds (for
//! Bluetooth devices connecting and disconnecting).

use std::ffi::c_void;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothDevice, BluetoothLEDevice};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Foundation::TypedEventHandler;
use windows::Networking::Connectivity::{NetworkInformation, NetworkStatusChangedEventHandler};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to the network and Bluetooth windows when anything changes.
pub const WM_CONNECTIVITY_UPDATED: u32 = WM_APP + 4;

/// How often to re-check even without a change notification.
const RECHECK: Duration = Duration::from_secs(5);

#[derive(Clone, Default, PartialEq)]
pub struct Status {
    /// Wi-Fi radio: None if this PC has no Wi-Fi.
    pub wifi_on: Option<bool>,
    /// A physical Ethernet cable is connected (virtual adapters such as VPNs don't count).
    pub wired: bool,
    /// Has a network connection at all.
    pub connected: bool,
    /// The Wi-Fi network's name (empty if Windows won't share it, e.g. location access off).
    pub ssid: String,
    /// Wi-Fi signal quality, 0..100.
    pub signal: u8,
    /// Bluetooth radio: None if this PC has no Bluetooth.
    pub bluetooth_on: Option<bool>,
    /// Names of connected Bluetooth devices.
    pub devices: Vec<String>,
}

enum Request {
    Refresh,
    SetWifi(bool),
    SetBluetooth(bool),
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);
static REQUESTS: OnceLock<Sender<Request>> = OnceLock::new();
static WINDOWS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

pub fn current() -> Status {
    STATUS.lock().unwrap().clone().unwrap_or_default()
}

pub fn set_wifi(on: bool) {
    send(Request::SetWifi(on));
}

pub fn set_bluetooth(on: bool) {
    send(Request::SetBluetooth(on));
}

fn send(r: Request) {
    if let Some(tx) = REQUESTS.get() {
        let _ = tx.send(r);
    }
}

/// Starts the background thread; `windows` are told (WM_CONNECTIVITY_UPDATED) about changes.
pub fn start(windows: &[HWND]) {
    *WINDOWS.lock().unwrap() = windows.iter().map(|h| h.0 as usize).collect();
    let (tx, rx) = channel();
    let _ = REQUESTS.set(tx.clone());
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        // Ask for permission to switch radios (desktop apps are normally allowed).
        let _ = Radio::RequestAccessAsync().and_then(|op| op.join());

        // Instant updates: network changes and radio switches.
        let tx_net = tx.clone();
        let _net = NetworkInformation::NetworkStatusChanged(&NetworkStatusChangedEventHandler::new(move |_| {
            let _ = tx_net.send(Request::Refresh);
            Ok(())
        }));
        let radios: Vec<Radio> = Radio::GetRadiosAsync()
            .and_then(|op| op.join())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        let mut _radio_tokens = Vec::new();
        for r in &radios {
            let tx_radio = tx.clone();
            if let Ok(t) = r.StateChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_radio.send(Request::Refresh);
                Ok(())
            })) {
                _radio_tokens.push(t);
            }
        }
        let radio = |kind: RadioKind| radios.iter().find(|r| r.Kind().ok() == Some(kind));

        loop {
            let status = read(radio(RadioKind::WiFi), radio(RadioKind::Bluetooth));
            let changed = {
                let mut s = STATUS.lock().unwrap();
                let changed = s.as_ref() != Some(&status);
                *s = Some(status);
                changed
            };
            if changed {
                for &h in WINDOWS.lock().unwrap().iter() {
                    unsafe {
                        let _ = PostMessageW(Some(HWND(h as *mut c_void)), WM_CONNECTIVITY_UPDATED, WPARAM(0), LPARAM(0));
                    }
                }
            }
            match rx.recv_timeout(RECHECK) {
                Ok(Request::SetWifi(on)) => set_radio(radio(RadioKind::WiFi), on),
                Ok(Request::SetBluetooth(on)) => set_radio(radio(RadioKind::Bluetooth), on),
                Ok(Request::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Several notifications often arrive together; read once for all of them.
            std::thread::sleep(Duration::from_millis(50));
            while let Ok(r) = rx.try_recv() {
                match r {
                    Request::SetWifi(on) => set_radio(radio(RadioKind::WiFi), on),
                    Request::SetBluetooth(on) => set_radio(radio(RadioKind::Bluetooth), on),
                    Request::Refresh => {}
                }
            }
        }
    });
}

fn set_radio(radio: Option<&Radio>, on: bool) {
    if let Some(r) = radio {
        let state = if on { RadioState::On } else { RadioState::Off };
        let _ = r.SetStateAsync(state).and_then(|op| op.join());
    }
}

fn read(wifi: Option<&Radio>, bluetooth: Option<&Radio>) -> Status {
    let on = |r: Option<&Radio>| r.map(|r| r.State().ok() == Some(RadioState::On));
    let mut status = Status { wifi_on: on(wifi), bluetooth_on: on(bluetooth), ..Default::default() };

    // The connection Windows uses for the internet: Wi-Fi or cable, name and signal.
    if let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() {
        status.connected = true;
        let _ = profile;
    }
    status.wired = ethernet_connected();
    if status.wired {
        status.connected = true;
    }
    // The Wi-Fi network's name and signal come from the classic Native Wi-Fi API (what
    // `netsh wlan` uses): the newer API above often hides them from desktop apps, and its
    // "is this Wi-Fi?" check isn't reliable either. Ask regardless of Ethernet: both can be
    // connected at once.
    if let Some((ssid, signal)) = wifi_details() {
        status.connected = true;
        status.ssid = ssid;
        status.signal = signal;
    }

    // Connected Bluetooth devices (classic and Low Energy), by name.
    if status.bluetooth_on == Some(true) {
        let mut names = Vec::new();
        let selectors = [
            BluetoothDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected),
            BluetoothLEDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected),
        ];
        for selector in selectors.into_iter().flatten() {
            if let Ok(found) = DeviceInformation::FindAllAsyncAqsFilter(&selector).and_then(|op| op.join()) {
                for d in found {
                    if let Ok(name) = d.Name() {
                        let name = name.to_string();
                        if !name.is_empty() && !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
            }
        }
        status.devices = names;
    }
    status
}


/// The connected Wi-Fi network's name and signal quality (0..100), via the Native Wi-Fi API.
fn wifi_details() -> Option<(String, u8)> {
    use windows::Win32::NetworkManagement::WiFi::*;
    unsafe {
        let mut version = 0u32;
        let mut handle = windows::Win32::Foundation::HANDLE::default();
        if WlanOpenHandle(2, None, &mut version, &mut handle) != 0 {
            return None;
        }
        let mut result = None;
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(handle, None, &mut list) == 0 && !list.is_null() {
            let count = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            for item in items {
                if item.isState != wlan_interface_state_connected {
                    continue;
                }
                let mut size = 0u32;
                let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
                if WlanQueryInterface(
                    handle,
                    &item.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    None,
                    &mut size,
                    &mut data,
                    None,
                ) == 0
                    && !data.is_null()
                {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    let assoc = &attrs.wlanAssociationAttributes;
                    let len = (assoc.dot11Ssid.uSSIDLength as usize).min(32);
                    let ssid = String::from_utf8_lossy(&assoc.dot11Ssid.ucSSID[..len]).to_string();
                    result = Some((ssid, assoc.wlanSignalQuality.min(100) as u8));
                    WlanFreeMemory(data);
                    break;
                }
            }
            WlanFreeMemory(list as *const std::ffi::c_void);
        }
        WlanCloseHandle(handle, None);
        result
    }
}



/// Whether a physical Ethernet adapter has a cable plugged in and is up. Virtual adapters
/// (VPN tunnels, Hyper-V switches) also call themselves Ethernet, so only real hardware counts.
fn ethernet_connected() -> bool {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    use windows::Win32::NetworkManagement::Ndis::{IfOperStatusUp, MediaConnectStateConnected};
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).is_err() || table.is_null() {
            return false;
        }
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let found = rows.iter().any(|r| {
            let hardware = r.InterfaceAndOperStatusFlags._bitfield & 1 != 0; // HardwareInterface
            r.Type == 6 // IF_TYPE_ETHERNET_CSMACD
                && hardware
                && r.OperStatus == IfOperStatusUp
                && r.MediaConnectState == MediaConnectStateConnected
        });
        FreeMibTable(table as *const std::ffi::c_void);
        found
    }
}
