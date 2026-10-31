//! Bluetooth side of the app.
//!
//! * Discovery uses a WinRT `DeviceWatcher` over Bluetooth Classic association endpoints.
//!   That returns both devices Windows has stored a pairing for *and* devices that are
//!   currently discoverable (a Switch controller is only discoverable in SYNC/pairing mode).
//! * "Connect" removes any stored pairing first (the link key Windows kept is usually stale
//!   once the controller has been synced with a Switch or another PC), then pairs fresh with
//!   `DeviceInformationCustomPairing`, accepting the "Just Works" prompt automatically.
//!   Windows then installs the HID gamepad driver exactly as it would from Settings.
//!
//! Everything blocking runs on one worker thread; results go back to the UI as `Event`s.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING};
use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationCustomPairing, DeviceInformationKind,
    DeviceInformationUpdate, DevicePairingKinds, DevicePairingRequestedEventArgs,
    DevicePairingResultStatus, DeviceUnpairingResultStatus, DeviceWatcher, DeviceWatcherStatus,
};
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};
use windows::Foundation::{IPropertyValue, TypedEventHandler};
use windows::Win32::Devices::Bluetooth::{
    BluetoothRemoveDevice, BluetoothUpdateDeviceRecord, BLUETOOTH_ADDRESS, BLUETOOTH_ADDRESS_0, BLUETOOTH_DEVICE_INFO,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_collections::IIterable;
use windows_future::{AsyncOperationCompletedHandler, IAsyncInfo, IAsyncOperation};

/// Bluetooth Classic (BR/EDR) AEP protocol id.
const BT_CLASSIC_PROTOCOL: &str = "{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}";
const P_ADDR: &str = "System.Devices.Aep.DeviceAddress";
const P_CONNECTED: &str = "System.Devices.Aep.IsConnected";
const P_PRESENT: &str = "System.Devices.Aep.IsPresent";
const P_RSSI: &str = "System.Devices.Aep.SignalStrength";
const P_PAIRED: &str = "System.Devices.Aep.IsPaired";
const P_CANPAIR: &str = "System.Devices.Aep.CanPair";

/// How long to keep retrying the pairing after the old pairing is removed.
/// A Switch controller stays in SYNC mode for roughly half a minute.
const PAIR_WINDOW: Duration = Duration::from_secs(45);

// ---------------------------------------------------------------------------------------------
// Messages between the UI and the worker
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub address: String,
    pub paired: bool,
    pub connected: bool,
    #[allow(dead_code)]
    pub present: bool,
    pub rssi: Option<i32>,
    pub is_switch: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioInfo {
    Unknown,
    On,
    Off,
    Missing,
}

pub enum Event {
    Clear,
    Upsert(DeviceRow),
    Remove(String),
    Status(String),
    Log(String),
    /// `Some(id)` while the worker is busy with that device, `None` when idle again.
    Busy(Option<String>),
    Finished { ok: bool, message: String },
    Radio(RadioInfo),
    /// `Some(reason)` when Bluetooth scanning couldn't start, `None` once it runs.
    ScanError(Option<String>),
}

pub enum Cmd {
    Rescan,
    /// Pair the device; `nickname` (if any) is written to Windows once it's paired.
    Connect { id: String, nickname: Option<String> },
    Forget(String),
    /// Write `windows_name` as the device's name in Windows' Bluetooth list.
    Rename { id: String, address: String, windows_name: String, message: String },
    Quit,
}

/// Sender back to the UI thread. `wake` pokes the UI so it drains the channel.
#[derive(Clone)]
pub struct Tx {
    tx: Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Tx {
    pub fn new(tx: Sender<Event>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Tx { tx, wake }
    }
    fn send(&self, e: Event) {
        if self.tx.send(e).is_ok() {
            (self.wake)();
        }
    }
    fn status(&self, s: impl Into<String>) {
        self.send(Event::Status(s.into()));
    }
    fn log(&self, s: impl Into<String>) {
        self.send(Event::Log(s.into()));
    }
}

pub fn spawn(tx: Tx) -> Sender<Cmd> {
    let (ctx, crx) = mpsc::channel();
    thread::Builder::new()
        .name("bluetooth".into())
        .spawn(move || worker(crx, tx))
        .expect("could not start Bluetooth worker thread");
    ctx
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn err(e: windows::core::Error) -> String {
    let msg = e.message();
    let msg = msg.trim();
    if msg.is_empty() {
        format!("error 0x{:08X}", e.code().0 as u32)
    } else {
        format!("{} (0x{:08X})", msg, e.code().0 as u32)
    }
}

/// Wait for a WinRT async operation with a timeout (cancels it on timeout).
fn wait<T: windows::core::RuntimeType + 'static>(op: IAsyncOperation<T>, timeout: Duration) -> Result<T, String> {
    let (tx, rx) = mpsc::channel::<()>();
    op.SetCompleted(&AsyncOperationCompletedHandler::<T>::new(move |_, _| {
        let _ = tx.send(());
        Ok(())
    }))
    .map_err(err)?;
    match rx.recv_timeout(timeout) {
        Ok(()) => op.GetResults().map_err(err),
        Err(_) => {
            if let Ok(info) = op.cast::<IAsyncInfo>() {
                let _ = info.Cancel();
            }
            Err("timed out".into())
        }
    }
}

fn props() -> IIterable<HSTRING> {
    IIterable::from(vec![
        HSTRING::from(P_ADDR),
        HSTRING::from(P_CONNECTED),
        HSTRING::from(P_PRESENT),
        HSTRING::from(P_RSSI),
        HSTRING::from(P_PAIRED),
        HSTRING::from(P_CANPAIR),
    ])
}

pub fn looks_like_switch(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "joy-con",
        "pro controller",
        "snes controller",
        "nes controller",
        "n64 controller",
        "md/gen",
        "nintendo",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// AEP ids look like `Bluetooth#Bluetooth00:1a:7d:da:71:13-98:b6:e9:12:34:56`.
fn address_from_id(id: &str) -> String {
    let tail = id.rsplit('-').next().unwrap_or("");
    if tail.len() == 17 && tail.matches(':').count() == 5 {
        tail.to_ascii_uppercase()
    } else {
        String::new()
    }
}

fn parse_address(addr: &str) -> Option<u64> {
    let hex: String = addr.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

impl DeviceRow {
    fn from_info(info: &DeviceInformation) -> Self {
        let id = info.Id().map(|h| h.to_string()).unwrap_or_default();
        let name = info.Name().map(|h| h.to_string()).unwrap_or_default();
        let map = info.Properties().ok();
        let get = |k: &str| -> Option<IPropertyValue> {
            map.as_ref()?.Lookup(&HSTRING::from(k)).ok()?.cast::<IPropertyValue>().ok()
        };
        let address = get(P_ADDR)
            .and_then(|v| v.GetString().ok())
            .map(|s| s.to_string().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| address_from_id(&id));
        let connected = get(P_CONNECTED).and_then(|v| v.GetBoolean().ok()).unwrap_or(false);
        let present = get(P_PRESENT).and_then(|v| v.GetBoolean().ok()).unwrap_or(false);
        let rssi = get(P_RSSI).and_then(|v| v.GetInt32().ok().or_else(|| v.GetInt16().ok().map(i32::from)));
        let paired = get(P_PAIRED)
            .and_then(|v| v.GetBoolean().ok())
            .or_else(|| info.Pairing().and_then(|p| p.IsPaired()).ok())
            .unwrap_or(false);
        let is_switch = looks_like_switch(&name);
        DeviceRow { id, name, address, paired, connected, present, rssi, is_switch }
    }
}

fn fetch(id: &str) -> Result<DeviceInformation, String> {
    let op = DeviceInformation::CreateFromIdAsyncWithKindAndAdditionalProperties(
        &HSTRING::from(id),
        &props(),
        DeviceInformationKind::AssociationEndpoint,
    )
    .map_err(err)?;
    wait(op, Duration::from_secs(10))
}

fn pair_status_text(s: DevicePairingResultStatus) -> String {
    use DevicePairingResultStatus as S;
    let t = match s {
        S::NotReadyToPair => "controller not found – is it in pairing mode?",
        S::NotPaired => "not paired",
        S::ConnectionRejected => "connection rejected by the controller",
        S::TooManyConnections => "too many Bluetooth connections",
        S::HardwareFailure => "Bluetooth adapter error",
        S::AuthenticationTimeout => "timed out waiting for the controller",
        S::AuthenticationNotAllowed => "authentication not allowed",
        S::AuthenticationFailure => "authentication failed",
        S::NoSupportedProfiles => "no supported profiles",
        S::ProtectionLevelCouldNotBeMet => "protection level not met",
        S::AccessDenied => "access denied",
        S::InvalidCeremonyData => "invalid pairing data",
        S::PairingCanceled => "pairing canceled",
        S::OperationAlreadyInProgress => "another pairing is already in progress",
        S::RequiredHandlerNotRegistered => "pairing handler missing",
        S::RejectedByHandler => "rejected",
        S::RemoteDeviceHasAssociation => "controller still has an old association",
        S::Failed => "no answer from the controller",
        other => return format!("pairing status {}", other.0),
    };
    t.to_string()
}

fn unpair_status_text(s: DeviceUnpairingResultStatus) -> String {
    use DeviceUnpairingResultStatus as S;
    match s {
        S::OperationAlreadyInProgress => "operation already in progress".into(),
        S::AccessDenied => "access denied".into(),
        S::Failed => "failed".into(),
        other => format!("status {}", other.0),
    }
}

// ---------------------------------------------------------------------------------------------
// Scanner (DeviceWatcher)
// ---------------------------------------------------------------------------------------------

struct Scanner {
    tx: Tx,
    watcher: Option<DeviceWatcher>,
    devices: Arc<Mutex<HashMap<String, DeviceInformation>>>,
    generation: Arc<AtomicU64>,
    last_start: Instant,
    reported_failure: bool,
}

impl Scanner {
    fn new(tx: Tx) -> Self {
        Scanner {
            tx,
            watcher: None,
            devices: Arc::new(Mutex::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
            last_start: Instant::now(),
            reported_failure: false,
        }
    }

    fn start(&mut self) {
        self.last_start = Instant::now();
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        match self.create_watcher(gen) {
            Ok(w) => {
                self.watcher = Some(w);
                if self.reported_failure {
                    self.tx.log("Bluetooth scanning resumed.");
                }
                self.reported_failure = false;
                self.tx.send(Event::ScanError(None));
            }
            Err(e) => {
                if !self.reported_failure {
                    self.tx.log(format!("Couldn't start Bluetooth scanning: {e}"));
                }
                self.reported_failure = true;
                self.tx.send(Event::ScanError(Some(e)));
            }
        }
    }

    fn create_watcher(&self, gen: u64) -> Result<DeviceWatcher, String> {
        let aqs = HSTRING::from(format!("System.Devices.Aep.ProtocolId:=\"{BT_CLASSIC_PROTOCOL}\""));
        let w = DeviceInformation::CreateWatcherWithKindAqsFilterAndAdditionalProperties(
            &aqs,
            &props(),
            DeviceInformationKind::AssociationEndpoint,
        )
        .map_err(err)?;

        {
            let (devs, tx, g) = (self.devices.clone(), self.tx.clone(), self.generation.clone());
            w.Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, info| {
                if g.load(Ordering::SeqCst) != gen {
                    return Ok(());
                }
                let info = info.ok()?;
                let row = DeviceRow::from_info(info);
                if let Ok(mut m) = devs.lock() {
                    m.insert(row.id.clone(), info.clone());
                }
                tx.send(Event::Upsert(row));
                Ok(())
            }))
            .map_err(err)?;
        }
        {
            let (devs, tx, g) = (self.devices.clone(), self.tx.clone(), self.generation.clone());
            w.Updated(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, upd| {
                if g.load(Ordering::SeqCst) != gen {
                    return Ok(());
                }
                let upd = upd.ok()?;
                let id = upd.Id()?.to_string();
                let row = match devs.lock() {
                    Ok(m) => match m.get(&id) {
                        Some(info) => {
                            let _ = info.Update(upd);
                            Some(DeviceRow::from_info(info))
                        }
                        None => None,
                    },
                    Err(_) => None,
                };
                if let Some(row) = row {
                    tx.send(Event::Upsert(row));
                }
                Ok(())
            }))
            .map_err(err)?;
        }
        {
            let (devs, tx, g) = (self.devices.clone(), self.tx.clone(), self.generation.clone());
            w.Removed(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, upd| {
                if g.load(Ordering::SeqCst) != gen {
                    return Ok(());
                }
                let id = upd.ok()?.Id()?.to_string();
                if let Ok(mut m) = devs.lock() {
                    m.remove(&id);
                }
                tx.send(Event::Remove(id));
                Ok(())
            }))
            .map_err(err)?;
        }
        {
            let (tx, g) = (self.tx.clone(), self.generation.clone());
            w.EnumerationCompleted(&TypedEventHandler::<DeviceWatcher, windows::core::IInspectable>::new(move |_, _| {
                if g.load(Ordering::SeqCst) == gen {
                    tx.log("Initial scan done – still listening for controllers in pairing mode.");
                }
                Ok(())
            }))
            .map_err(err)?;
        }
        w.Start().map_err(err)?;
        Ok(w)
    }

    fn stop(&mut self) {
        if let Some(w) = self.watcher.take() {
            if let Ok(st) = w.Status() {
                if st == DeviceWatcherStatus::Started || st == DeviceWatcherStatus::EnumerationCompleted {
                    let _ = w.Stop();
                }
            }
        }
    }

    fn restart(&mut self, clear: bool) {
        self.stop();
        if clear {
            if let Ok(mut m) = self.devices.lock() {
                m.clear();
            }
            self.tx.send(Event::Clear);
        }
        self.start();
    }

    /// Restart the watcher if Windows stopped it (e.g. Bluetooth was switched off and on).
    fn keep_alive(&mut self) {
        let alive = matches!(
            self.watcher.as_ref().and_then(|w| w.Status().ok()),
            Some(DeviceWatcherStatus::Started) | Some(DeviceWatcherStatus::EnumerationCompleted) | Some(DeviceWatcherStatus::Created)
        );
        if !alive && self.last_start.elapsed() > Duration::from_secs(8) {
            self.restart(false);
        }
    }

    /// Re-read one device after a connect/forget so the list shows its new state right away.
    fn refresh(&self, id: &str) {
        if let Ok(info) = fetch(id) {
            let row = DeviceRow::from_info(&info);
            if let Ok(mut m) = self.devices.lock() {
                m.insert(id.to_string(), info);
            }
            self.tx.send(Event::Upsert(row));
        }
    }

    fn cached(&self, id: &str) -> Option<DeviceInformation> {
        self.devices.lock().ok()?.get(id).cloned()
    }
}

// ---------------------------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------------------------

fn worker(rx: Receiver<Cmd>, tx: Tx) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut scanner = Scanner::new(tx.clone());
    let mut radio = check_radio(&tx, false);
    let mut radio_checked = Instant::now();
    scanner.start();

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                // If Bluetooth was off/missing, notice when it comes back.
                if radio == RadioInfo::Off && radio_checked.elapsed() > Duration::from_secs(6) {
                    let now = check_radio(&tx, false);
                    radio_checked = Instant::now();
                    if now == RadioInfo::On && radio != RadioInfo::On {
                        tx.log("Bluetooth is on.");
                        scanner.restart(true);
                    }
                    radio = now;
                }
                scanner.keep_alive();
            }
            Ok(Cmd::Rescan) => {
                radio = check_radio(&tx, true);
                radio_checked = Instant::now();
                scanner.restart(true);
                tx.log("Rescanning…");
            }
            Ok(Cmd::Rename { id, address, windows_name, message }) => {
                tx.send(Event::Busy(Some(id.clone())));
                let r = set_windows_name(&address, &windows_name);
                scanner.refresh(&id);
                tx.send(Event::Busy(None));
                let (ok, m) = match r {
                    Ok(()) => (true, message),
                    Err(e) => (
                        false,
                        format!("Saved the nickname in this app, but Windows refused to rename the device ({e}). It will still show here."),
                    ),
                };
                tx.log(m.clone());
                tx.send(Event::Finished { ok, message: m });
            }
            Ok(Cmd::Connect { id, nickname }) => {
                tx.send(Event::Busy(Some(id.clone())));
                radio = check_radio(&tx, true);
                radio_checked = Instant::now();
                if radio == RadioInfo::Off {
                    tx.send(Event::Busy(None));
                    tx.send(Event::Finished { ok: false, message: "Bluetooth is turned off. Turn it on in Windows Settings, then try again.".into() });
                    continue;
                }
                let r = connect(&scanner, &tx, &id, nickname.as_deref());
                scanner.refresh(&id);
                tx.send(Event::Busy(None));
                match r {
                    Ok(m) => {
                        tx.log(m.clone());
                        tx.send(Event::Finished { ok: true, message: m });
                    }
                    Err(m) => {
                        tx.log(m.clone());
                        tx.send(Event::Finished { ok: false, message: m });
                    }
                }
            }
            Ok(Cmd::Forget(id)) => {
                tx.send(Event::Busy(Some(id.clone())));
                let r = forget(&scanner, &tx, &id);
                scanner.refresh(&id);
                tx.send(Event::Busy(None));
                let (ok, m) = match r {
                    Ok(m) => (true, m),
                    Err(m) => (false, m),
                };
                tx.log(m.clone());
                tx.send(Event::Finished { ok, message: m });
            }
        }
    }
    scanner.stop();
}

/// Reports the Bluetooth radio state; optionally tries to switch it on.
fn check_radio(tx: &Tx, try_enable: bool) -> RadioInfo {
    let radios = match Radio::GetRadiosAsync().map_err(err).and_then(|op| wait(op, Duration::from_secs(5))) {
        Ok(r) => r,
        Err(e) => {
            tx.log(format!("Couldn't read the Bluetooth radio state: {e}"));
            tx.send(Event::Radio(RadioInfo::Unknown));
            return RadioInfo::Unknown;
        }
    };
    let mut bt = None;
    let n = radios.Size().unwrap_or(0);
    for i in 0..n {
        if let Ok(r) = radios.GetAt(i) {
            if r.Kind().map(|k| k == RadioKind::Bluetooth).unwrap_or(false) {
                bt = Some(r);
                break;
            }
        }
    }
    let info = match bt {
        None => RadioInfo::Missing,
        Some(r) => {
            let mut on = r.State().map(|s| s == RadioState::On).unwrap_or(true);
            if !on && try_enable {
                tx.log("Bluetooth is off – trying to turn it on…");
                let allowed = Radio::RequestAccessAsync()
                    .map_err(err)
                    .and_then(|op| wait(op, Duration::from_secs(10)))
                    .map(|a| a == RadioAccessStatus::Allowed)
                    .unwrap_or(false);
                if allowed {
                    let set = r
                        .SetStateAsync(RadioState::On)
                        .map_err(err)
                        .and_then(|op| wait(op, Duration::from_secs(10)))
                        .map(|a| a == RadioAccessStatus::Allowed)
                        .unwrap_or(false);
                    if set {
                        thread::sleep(Duration::from_secs(2));
                        on = r.State().map(|s| s == RadioState::On).unwrap_or(false);
                    }
                }
                tx.log(if on { "Bluetooth turned on." } else { "Couldn't turn Bluetooth on automatically." });
            }
            if on {
                RadioInfo::On
            } else {
                RadioInfo::Off
            }
        }
    };
    tx.send(Event::Radio(info));
    info
}

fn display_name(row: &DeviceRow) -> String {
    if row.name.trim().is_empty() {
        "the controller".into()
    } else {
        row.name.clone()
    }
}

/// Remove Windows' stored pairing (WinRT first, Win32 `BluetoothRemoveDevice` as fallback).
fn unpair(tx: &Tx, info: &DeviceInformation, address: &str) -> Result<(), String> {
    let r = info
        .Pairing()
        .and_then(|p| p.UnpairAsync())
        .map_err(err)
        .and_then(|op| wait(op, Duration::from_secs(20)))
        .and_then(|res| res.Status().map_err(err));
    match r {
        Ok(s) if s == DeviceUnpairingResultStatus::Unpaired || s == DeviceUnpairingResultStatus::AlreadyUnpaired => {
            tx.log("Removed the old pairing.");
            return Ok(());
        }
        Ok(s) => tx.log(format!("Windows couldn't remove the pairing ({}) – trying another way…", unpair_status_text(s))),
        Err(e) => tx.log(format!("Unpair failed ({e}) – trying another way…")),
    }
    let Some(a) = parse_address(address) else {
        return Err("Couldn't remove the old pairing. Remove the controller in Settings > Bluetooth & devices, then try again.".into());
    };
    let addr = BLUETOOTH_ADDRESS { Anonymous: BLUETOOTH_ADDRESS_0 { ullLong: a } };
    let code = unsafe { BluetoothRemoveDevice(&addr) };
    // 0 = success, 1168 = ERROR_NOT_FOUND (already gone)
    if code == 0 || code == 1168 {
        tx.log("Removed the old pairing (fallback method).");
        Ok(())
    } else {
        Err(format!(
            "Couldn't remove the old pairing (error {code}). Remove the controller in Settings > Bluetooth & devices, then try again."
        ))
    }
}

/// Rename a paired device in Windows' Bluetooth list (the same thing Control Panel's rename does).
/// Only the address and name fields are used by `BluetoothUpdateDeviceRecord`.
fn set_windows_name(address: &str, name: &str) -> Result<(), String> {
    let a = parse_address(address).ok_or("unknown Bluetooth address")?;
    // The demo build runs under Wine for UI testing, which doesn't implement this API.
    #[cfg(feature = "demo")]
    if a != 0 {
        return Err("skipped in demo build".into());
    }
    let mut info = BLUETOOTH_DEVICE_INFO::default();
    info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
    info.Address = BLUETOOTH_ADDRESS { Anonymous: BLUETOOTH_ADDRESS_0 { ullLong: a } };
    let wide: Vec<u16> = name.encode_utf16().take(info.szName.len() - 1).collect();
    info.szName[..wide.len()].copy_from_slice(&wide);
    match unsafe { BluetoothUpdateDeviceRecord(&info) } {
        0 => Ok(()),
        87 => Err("invalid parameter".into()),
        1168 => Err("Windows doesn't have this device paired".into()),
        code => Err(format!("error {code}")),
    }
}

fn pair_once(info: &DeviceInformation) -> Result<DevicePairingResultStatus, String> {
    let custom = info.Pairing().and_then(|p| p.Custom()).map_err(err)?;
    let handler = TypedEventHandler::<DeviceInformationCustomPairing, DevicePairingRequestedEventArgs>::new(|_, args| {
        let args = args.ok()?;
        if args.PairingKind()? == DevicePairingKinds::ProvidePin {
            args.AcceptWithPin(&HSTRING::from("0000"))
        } else {
            // ConfirmOnly ("Just Works"), ConfirmPinMatch and DisplayPin: just accept.
            args.Accept()
        }
    });
    let token = custom.PairingRequested(&handler).map_err(err)?;
    let kinds = DevicePairingKinds::ConfirmOnly
        | DevicePairingKinds::ConfirmPinMatch
        | DevicePairingKinds::DisplayPin
        | DevicePairingKinds::ProvidePin;
    let result = custom
        .PairAsync(kinds)
        .map_err(err)
        .and_then(|op| wait(op, Duration::from_secs(40)));
    let _ = custom.RemovePairingRequested(token);
    result.and_then(|r| r.Status().map_err(err))
}

fn connect(scanner: &Scanner, tx: &Tx, id: &str, nickname: Option<&str>) -> Result<String, String> {
    let info = match scanner.cached(id) {
        Some(i) => i,
        None => fetch(id).map_err(|e| format!("Couldn't find that controller anymore ({e}). Click Rescan and try again."))?,
    };
    let row = DeviceRow::from_info(&info);
    let name = nickname.map(str::to_string).unwrap_or_else(|| display_name(&row));
    tx.log(format!("Connecting to {name} [{}]…", row.address));

    let paired = info.Pairing().and_then(|p| p.IsPaired()).unwrap_or(row.paired);
    if paired && row.connected {
        return Ok(format!("{name} is already connected. If it isn't responding, click Forget pairing, put it in pairing mode and Connect again."));
    }
    if paired {
        tx.status(format!("Removing the old pairing for {name}…"));
        unpair(tx, &info, &row.address)?;
        thread::sleep(Duration::from_millis(1500));
    }

    tx.status(format!("Pairing with {name}… keep it in pairing mode (lights running back and forth)."));
    let deadline = Instant::now() + PAIR_WINDOW;
    let mut attempt = 0;
    let mut last: String;
    let mut removed_again = false;
    loop {
        attempt += 1;
        let r = fetch(id).and_then(|i| pair_once(&i).map(|s| (s, i)));
        match r {
            Ok((s, _)) if s == DevicePairingResultStatus::Paired => break,
            Ok((s, i)) if s == DevicePairingResultStatus::AlreadyPaired => {
                if paired && !removed_again {
                    // Windows still reports the old pairing: remove it once more, then retry.
                    removed_again = true;
                    tx.log("Windows still had the old pairing – removing it again…");
                    let _ = unpair(tx, &i, &row.address);
                    thread::sleep(Duration::from_millis(1500));
                    continue;
                }
                break;
            }
            Ok((s, _)) => last = pair_status_text(s),
            Err(e) => last = e,
        }
        tx.log(format!("Attempt {attempt}: {last}"));
        if Instant::now() >= deadline {
            return Err(format!(
                "Couldn't pair with {name} ({last}). Hold the SYNC button until the lights run back and forth, then click Connect again."
            ));
        }
        if attempt == 2 {
            tx.status("Still trying… if the lights stopped running, press and hold SYNC again.");
        }
        thread::sleep(Duration::from_millis(1200));
    }

    // A fresh pairing starts with the controller's own name; put the nickname back.
    if let Some(nick) = nickname {
        thread::sleep(Duration::from_millis(500));
        match set_windows_name(&row.address, nick) {
            Ok(()) => tx.log(format!("Named it \"{nick}\" in Windows.")),
            Err(e) => tx.log(format!("Couldn't apply the nickname in Windows ({e}).")),
        }
    }

    tx.status(format!("Paired with {name}. Waiting for it to connect…"));
    let until = Instant::now() + Duration::from_secs(12);
    while Instant::now() < until {
        if let Ok(i) = fetch(id) {
            if DeviceRow::from_info(&i).connected {
                return Ok(format!("Done! {name} is connected."));
            }
        }
        thread::sleep(Duration::from_millis(800));
    }
    Ok(format!("Done! {name} is paired. If it doesn't respond yet, press any button on it."))
}

fn forget(scanner: &Scanner, tx: &Tx, id: &str) -> Result<String, String> {
    let info = match scanner.cached(id) {
        Some(i) => i,
        None => fetch(id).map_err(|e| format!("Couldn't find that device anymore ({e}). Click Rescan and try again."))?,
    };
    let row = DeviceRow::from_info(&info);
    let name = display_name(&row);
    if !info.Pairing().and_then(|p| p.IsPaired()).unwrap_or(row.paired) {
        return Ok(format!("Windows has no saved pairing for {name}."));
    }
    tx.status(format!("Removing the saved pairing for {name}…"));
    unpair(tx, &info, &row.address)?;
    Ok(format!("Removed {name} from Windows. Put it in pairing mode and click Connect."))
}
