use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use rusb::{DeviceHandle, UsbContext};

use crate::device::UpgDevice;
use crate::log_verbose;
use crate::protocol::cbw_csw::*;
use crate::transport::{CswPolicy, UpgTransport};

const AIC_VID: u16 = 0x33C3;
const AIC_PID: u16 = 0x6677;
const BULK_OUT_EP: u8 = 0x02;
const BULK_IN_EP: u8 = 0x81;
const TIMEOUT_MS: Duration = Duration::from_secs(30);
const SHORT_TIMEOUT: Duration = Duration::from_millis(500);

/// Bulk-transfer ceiling, overridable for fast failure while debugging:
/// `ARTINCHIP_FLASH_TIMEOUT_MS` (milliseconds, floor 500ms, default 30000).
/// Short timeouts only change how long we wait, never the wire bytes.
fn bulk_timeout() -> Duration {
    parse_timeout_ms(std::env::var("ARTINCHIP_FLASH_TIMEOUT_MS").ok().as_deref())
}

/// Pure parser for [`bulk_timeout`] (kept separate so unit tests don't have
/// to mutate the process-global environment).
fn parse_timeout_ms(var: Option<&str>) -> Duration {
    var.and_then(|text| text.trim().parse::<u64>().ok())
        .map(|ms| ms.max(500))
        .map(Duration::from_millis)
        .unwrap_or(TIMEOUT_MS)
}
const BULK_WRITE_CHUNK: usize = 64 * 1024;
const RECONNECT_SETTLE_DELAY: Duration = Duration::from_millis(120);
const START_WRITE_RETRY_DELAY: Duration = Duration::from_millis(100);
const START_WRITE_RETRY_TIMEOUT: Duration = Duration::from_secs(5);

pub use crate::device::{BurnCallback, BurnEvent, BurnOptions};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub bus_number: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub port_path: String,
    pub speed: String,
    /// The device was enumerated by libusb, so selecting it and attempting a
    /// real `device.open()` is allowed.  This is deliberately *not* protocol
    /// readiness: IOKit may still refuse the open, and no UPG query has run.
    pub ready: bool,
    /// IOKit's configured/interface view on macOS.  Diagnostic only; it never
    /// overrides libusb enumeration or gates an open attempt.
    pub ioreg_ready: Option<bool>,
    /// Protocol readiness from an actual interface claim + UPG query.
    /// `None` means "not probed yet"; only a successful probe sets `Some(true)`.
    pub protocol_ready: Option<bool>,
    /// Latest diagnostic detail (for example a macOS IOKit state warning).
    pub status: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    pub bus_number: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub port_path: String,
    pub speed: String,
    pub class_code: u8,
    pub subclass_code: u8,
    pub protocol_code: u8,
}

/// How to pick one ArtInChip device when several are attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceSelector {
    /// Exactly one device must be present.
    Unique,
    /// Stable physical USB port path, e.g. `9-2`.
    PortPath(String),
    PhysicalPort(u8, String),
    /// Volatile bus:address pair, e.g. `1:45`.
    BusAddress(u8, u8),
}

impl DeviceSelector {
    /// Parse `--device` (`port-path` or `bus:address`); `None` means Unique.
    pub fn parse(selector: Option<&str>) -> Result<Self, String> {
        let Some(text) = selector.map(str::trim).filter(|text| !text.is_empty()) else {
            return Ok(Self::Unique);
        };
        if let Some((bus, address)) = text.split_once(':') {
            let bus = bus
                .parse::<u8>()
                .map_err(|_| format!("Invalid bus number in '{}'", text))?;
            let address = address
                .parse::<u8>()
                .map_err(|_| format!("Invalid address in '{}'", text))?;
            return Ok(Self::BusAddress(bus, address));
        }
        Ok(Self::PortPath(text.to_string()))
    }
}

/// Pick the single candidate matching `selector`.
fn select_candidate(
    selector: &DeviceSelector,
    candidates: &[(u8, u8, String)],
) -> Result<(u8, u8), String> {
    let describe = |candidates: &[(u8, u8, String)]| {
        candidates
            .iter()
            .map(|(bus, address, path)| {
                format!(
                    "bus {} address {} path {}",
                    bus,
                    address,
                    if path.is_empty() { "?" } else { path }
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };

    match selector {
        DeviceSelector::Unique => match candidates {
            [] => Err("No ArtInChip device found (VID=0x33C3, PID=0x6677)".to_string()),
            [one] => Ok((one.0, one.1)),
            many => Err(format!(
                "Multiple ArtInChip devices found; pass --device <port-path|bus:address>: {}",
                describe(many)
            )),
        },
        DeviceSelector::PortPath(path) | DeviceSelector::PhysicalPort(_, path) => {
            let found: Vec<_> = candidates
                .iter()
                .filter(|(bus, _, candidate_path)| {
                    !path.is_empty()
                        && candidate_path == path
                        && !matches!(selector, DeviceSelector::PhysicalPort(wanted, _) if wanted != bus)
                })
                .collect();
            match found.as_slice() {
                [one] => Ok((one.0, one.1)),
                [] => Err(format!("No ArtInChip device at port path '{}'", path)),
                _ => Err(format!(
                    "Multiple ArtInChip devices at port path '{}'",
                    path
                )),
            }
        }
        DeviceSelector::BusAddress(bus, address) => {
            let found: Vec<_> = candidates
                .iter()
                .filter(|(candidate_bus, candidate_address, _)| {
                    candidate_bus == bus && candidate_address == address
                })
                .collect();
            match found.as_slice() {
                [one] => Ok((one.0, one.1)),
                [] => Err(format!(
                    "No ArtInChip device at bus {} address {}",
                    bus, address
                )),
                _ => Err(format!(
                    "Multiple ArtInChip devices at bus {} address {}",
                    bus, address
                )),
            }
        }
    }
}

fn pin_candidate(
    selector: &DeviceSelector,
    candidates: &[(u8, u8, String)],
) -> Result<DeviceSelector, String> {
    let (bus, address) = select_candidate(selector, candidates)?;
    let path = candidates
        .iter()
        .find(|candidate| (candidate.0, candidate.1) == (bus, address))
        .map(|candidate| candidate.2.as_str())
        .filter(|path| !path.is_empty())
        .ok_or("Selected USB device has no stable physical port path")?;
    Ok(DeviceSelector::PhysicalPort(bus, path.to_owned()))
}

/// USB transport for the ArtInChip UPG protocol (CBW/CSW over bulk endpoints).
pub struct UsbTransport {
    handle: DeviceHandle<rusb::Context>,
    _access_lock: UsbAccessLock,
    tag: u32,
    in_buf: Vec<u8>,
    bus_number: u8,
    address: u8,
    /// Stable physical USB port path (e.g. "9-2") used to re-find the
    /// device after the updater re-enumerates it.
    port_path: String,
}

/// A UPG device bound to a claimed USB interface and its physical-port lock.
pub struct AicDevice(UpgDevice<UsbTransport>);

impl std::ops::Deref for AicDevice {
    type Target = UpgDevice<UsbTransport>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for AicDevice {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AicDevice {
    /// Erase the transport type while retaining the USB session and lock.
    pub fn into_boxed(self) -> UpgDevice<Box<dyn UpgTransport>> {
        self.0.into_boxed()
    }

    /// Resolve a selector to one stable physical port without claiming the USB interface.
    pub fn pin_selector(selector: &DeviceSelector) -> Result<DeviceSelector, String> {
        let candidates: Vec<_> = scan_devices_libusb()?
            .into_iter()
            .map(|device| (device.bus_number, device.address, device.port_path))
            .collect();
        pin_candidate(selector, &candidates)
    }

    pub fn list_usb_devices() -> Result<Vec<UsbDeviceInfo>, String> {
        let context = rusb::Context::new().map_err(|e| format!("Failed to init USB: {}", e))?;
        let devices = context
            .devices()
            .map_err(|e| format!("Failed to list USB devices: {}", e))?;

        let mut found = Vec::new();
        for device in devices.iter() {
            let desc = device
                .device_descriptor()
                .map_err(|e| format!("Failed to get device descriptor: {}", e))?;
            let port_path = device
                .port_numbers()
                .map(|ports| {
                    ports
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join("-")
                })
                .unwrap_or_default();
            found.push(UsbDeviceInfo {
                bus_number: device.bus_number(),
                address: device.address(),
                vendor_id: desc.vendor_id(),
                product_id: desc.product_id(),
                port_path,
                speed: format!("{:?}", device.speed()),
                class_code: desc.class_code(),
                subclass_code: desc.sub_class_code(),
                protocol_code: desc.protocol_code(),
            });
        }
        Ok(found)
    }

    pub fn scan_devices() -> Result<Vec<DeviceInfo>, String> {
        let devices = scan_devices_libusb()?;
        #[cfg(target_os = "macos")]
        let devices = {
            let mut devices = devices;
            apply_macos_diagnostics(&mut devices);
            devices
        };
        Ok(devices)
    }

    pub fn scan_devices_libusb() -> Result<Vec<DeviceInfo>, String> {
        scan_devices_libusb()
    }

    /// Devices that can be selected for a real open attempt.  libusb
    /// enumeration is authoritative; macOS IOKit state is attached as a
    /// diagnostic only, never as a hard `ready=false` override.
    pub fn scan_selectable_devices() -> Result<Vec<DeviceInfo>, String> {
        Self::scan_devices()
    }

    pub fn scan_devices_fast() -> Result<Vec<DeviceInfo>, String> {
        #[cfg(target_os = "macos")]
        {
            scan_devices_macos_ioreg()
        }
        #[cfg(not(target_os = "macos"))]
        {
            scan_devices_libusb()
        }
    }

    pub fn open_first() -> Result<Self, String> {
        Self::open_selector(DeviceSelector::Unique)
    }

    pub fn open_by_location(bus_number: u8, address: u8) -> Result<Self, String> {
        Self::open_selector(DeviceSelector::BusAddress(bus_number, address))
    }

    pub fn open_by_port_path(path: &str) -> Result<Self, String> {
        Self::open_selector(DeviceSelector::PortPath(path.to_string()))
    }

    /// Open the requested device; `None` requires exactly one candidate.
    pub fn open_selected(selector: Option<&str>) -> Result<Self, String> {
        Self::open_selector(DeviceSelector::parse(selector)?)
    }

    pub fn open_selector(selector: DeviceSelector) -> Result<Self, String> {
        Self::open_matching_with_recovery(selector, false)
    }

    fn open_matching_with_recovery(
        selector: DeviceSelector,
        recover_endpoints: bool,
    ) -> Result<Self, String> {
        let access_lock = UsbAccessLock::try_acquire()?;
        Self::open_matching_with_lock(selector, recover_endpoints, access_lock)
    }

    fn open_matching_with_lock(
        selector: DeviceSelector,
        recover_endpoints: bool,
        access_lock: UsbAccessLock,
    ) -> Result<Self, String> {
        let transport =
            UsbTransport::open_matching_with_lock(selector, recover_endpoints, access_lock)?;
        Ok(Self(UpgDevice::new(transport)))
    }
}

impl UsbTransport {
    fn open_matching_with_lock(
        selector: DeviceSelector,
        recover_endpoints: bool,
        access_lock: UsbAccessLock,
    ) -> Result<Self, String> {
        let context = rusb::Context::new().map_err(|e| format!("Failed to init USB: {}", e))?;
        let devices = context
            .devices()
            .map_err(|e| format!("Failed to list USB devices: {}", e))?;

        let mut candidates: Vec<(u8, u8, String)> = Vec::new();
        for device in devices.iter() {
            let desc = device
                .device_descriptor()
                .map_err(|e| format!("Failed to get device descriptor: {}", e))?;
            if desc.vendor_id() == AIC_VID && desc.product_id() == AIC_PID {
                let port_path = device
                    .port_numbers()
                    .map(|ports| {
                        ports
                            .iter()
                            .map(u8::to_string)
                            .collect::<Vec<_>>()
                            .join("-")
                    })
                    .unwrap_or_default();
                candidates.push((device.bus_number(), device.address(), port_path));
            }
        }
        let wanted = select_candidate(&selector, &candidates)?;
        log_verbose!(
            "Selected ArtInChip device at bus {} address {} ({} candidate(s))",
            wanted.0,
            wanted.1,
            candidates.len()
        );

        for device in devices.iter() {
            let desc = device
                .device_descriptor()
                .map_err(|e| format!("Failed to get device descriptor: {}", e))?;
            if desc.vendor_id() != AIC_VID || desc.product_id() != AIC_PID {
                continue;
            }
            if (device.bus_number(), device.address()) != wanted {
                continue;
            }

            // Attempt the real libusb open first.  macOS IOKit state is only
            // diagnostic: it must never replace the libusb error (or success).
            let handle = match device.open() {
                Ok(handle) => handle,
                Err(error) => {
                    let config_error = device.config_descriptor(0).err();
                    let active_error = device.active_config_descriptor().err();
                    let mut detail = format_usb_open_error(&error, device.address());
                    if let Some(config_error) = config_error {
                        detail.push_str(&format!(
                            "; configuration descriptor 0 also failed: {}",
                            format_usb_error(&config_error)
                        ));
                    }
                    if let Some(active_error) = active_error {
                        detail.push_str(&format!(
                            "; active configuration descriptor also failed: {}",
                            format_usb_error(&active_error)
                        ));
                    }
                    return Err(format!(
                        "Found ArtInChip device at bus {} address {}, but failed to open it: {}",
                        device.bus_number(),
                        device.address(),
                        detail
                    ));
                }
            };

            let config = device.config_descriptor(0).map_err(|error| {
                format!(
                    "Opened ArtInChip device at bus {} address {}, but libusb could not read configuration descriptor 0: {}. This points to a device-side USB descriptor/configuration problem or an OS enumeration failure.",
                    device.bus_number(),
                    device.address(),
                    format_usb_error(&error)
                )
            })?;
            validate_configuration_endpoints(&config)?;
            log_configuration_descriptor(&config);
            match device.active_config_descriptor() {
                Ok(active) => log_configuration_descriptor(&active),
                Err(error) => log_verbose!(
                    "  Active configuration descriptor unavailable: {}",
                    format_usb_error(&error)
                ),
            }

            if let Ok(active) = handle.kernel_driver_active(0) {
                if active {
                    let _ = handle.detach_kernel_driver(0);
                }
            }

            handle.claim_interface(0).map_err(|error| {
                format!(
                    "Failed to claim interface 0 on ArtInChip device bus {} address {}: {}. The device may be held by another process/driver.",
                    device.bus_number(),
                    device.address(),
                    format_usb_error(&error)
                )
            })?;

            let port_path = device
                .port_numbers()
                .map(|ports| {
                    ports
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join("-")
                })
                .unwrap_or_default();
            let mut transport = Self {
                handle,
                _access_lock: access_lock,
                tag: 1,
                in_buf: Vec::new(),
                bus_number: device.bus_number(),
                address: device.address(),
                port_path,
            };
            if recover_endpoints {
                let _ = transport.handle.clear_halt(BULK_OUT_EP);
                let _ = transport.handle.clear_halt(BULK_IN_EP);
                log_verbose!(
                    "  Cleared halt on EP 0x{:02x} and 0x{:02x}",
                    BULK_OUT_EP,
                    BULK_IN_EP
                );
                transport.drain_in_endpoint(Duration::from_millis(50), 64 * 1024)?;
            }
            return Ok(transport);
        }
        Err(format!(
            "ArtInChip device disappeared before it could be opened (bus {} address {})",
            wanted.0, wanted.1
        ))
    }

    fn reopen(&mut self) -> Result<(), String> {
        // Re-select by the stable physical port path.  Never fall back to
        // "first device": that could silently burn a different board.
        if self.port_path.is_empty() {
            return Err("Cannot reconnect safely without a physical USB port path".to_string());
        }
        let selector = DeviceSelector::PhysicalPort(self.bus_number, self.port_path.clone());
        let replacement =
            UsbTransport::open_matching_with_lock(selector, false, self._access_lock.clone())?;
        if !self.port_path.is_empty() && replacement.port_path != self.port_path {
            return Err(format!(
                "Device reconnected on a different port path: {} -> {}",
                self.port_path, replacement.port_path
            ));
        }
        *self = replacement;
        Ok(())
    }

    fn has_device_at(bus_number: u8, address: u8) -> Result<bool, String> {
        let context = rusb::Context::new().map_err(|e| format!("Failed to init USB: {}", e))?;
        let devices = context
            .devices()
            .map_err(|e| format!("Failed to list USB devices: {}", e))?;

        for device in devices.iter() {
            let desc = device
                .device_descriptor()
                .map_err(|e| format!("Failed to get device descriptor: {}", e))?;
            if desc.vendor_id() == AIC_VID
                && desc.product_id() == AIC_PID
                && device.bus_number() == bus_number
                && device.address() == address
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn wait_reconnect(&mut self, timeout: Duration) -> Result<(), String> {
        log_verbose!("Waiting for ArtInChip device to reconnect...");
        let old_bus = self.bus_number;
        let old_address = self.address;
        let deadline = Instant::now() + timeout;
        let mut last_err = String::new();
        let mut old_device_gone = false;
        while Instant::now() < deadline {
            if !old_device_gone {
                match Self::has_device_at(old_bus, old_address) {
                    Ok(false) => {
                        old_device_gone = true;
                        log_verbose!("  Previous device {}:{} disappeared", old_bus, old_address);
                    }
                    Ok(true) => {
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    Err(e) => last_err = e,
                }
            }

            if old_device_gone {
                match self.reopen() {
                    Ok(()) => {
                        log_verbose!(
                            "Device reconnected at {}:{}.",
                            self.bus_number,
                            self.address
                        );
                        thread::sleep(RECONNECT_SETTLE_DELAY);
                        return Ok(());
                    }
                    Err(e) => last_err = e,
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "Timed out waiting for device reconnect: {}",
            last_err
        ))
    }

    fn drain_in_endpoint(&mut self, timeout: Duration, max_bytes: usize) -> Result<(), String> {
        let mut total = 0usize;
        let mut buf = [0u8; 512];
        loop {
            match self.handle.read_bulk(BULK_IN_EP, &mut buf, timeout) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    total += n;
                    log_verbose!("  Flushed {} stale bytes from IN EP", n);
                    if total >= max_bytes {
                        log_verbose!("  Stopped IN flush after {} bytes", total);
                        break;
                    }
                }
                Err(rusb::Error::Timeout) => break,
                Err(e) => return Err(format!("IN EP flush error: {}", e)),
            }
        }
        Ok(())
    }

    fn next_tag(&mut self) -> u32 {
        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1).max(1);
        tag
    }

    // ── Low-level transactions ─────────────────────────────────────────

    fn write_txn_inner(
        &mut self,
        payload: &[u8],
        policy: CswPolicy,
    ) -> Result<Option<AicCsw>, String> {
        let tag = self.next_tag();

        let cbw = AicCbw::new_write(tag, payload.len() as u32);
        let cbw_bytes = cbw.to_bytes();
        log_verbose!(
            "  >> WRITE CBW tag={} len={} cbw={:02x?}",
            tag,
            payload.len(),
            cbw_bytes
        );
        self.write_bulk(cbw_bytes)?;
        if !payload.is_empty() {
            log_verbose!("  >> DATA len={}", payload.len());
            self.write_bulk_data_phase(payload)?;
        }
        let csw = self.read_csw(tag, policy)?;
        if let Some(csw) = &csw {
            log_verbose!(
                "  << CSW tag={} status={} residue={} sig=0x{:08x}",
                csw.tag_val(),
                csw.status_val(),
                csw.data_residue_val(),
                csw.signature()
            );
            self.check_csw(csw, tag)?;
        }
        Ok(csw)
    }

    fn read_txn_inner(&mut self, read_len: u32, policy: CswPolicy) -> Result<Vec<u8>, String> {
        let tag = self.next_tag();

        let cbw = AicCbw::new_read(tag, read_len);
        log_verbose!(
            "  >> READ CBW tag={} len={} cbw={:02x?}",
            tag,
            read_len,
            cbw.to_bytes()
        );
        self.write_bulk(cbw.to_bytes())?;

        let data = self.read_exact_from_in(read_len as usize, bulk_timeout())?;
        log_verbose!("  << DATA {} bytes", data.len());

        let csw = self.read_csw(tag, policy)?;
        if let Some(csw) = &csw {
            log_verbose!(
                "  << CSW tag={} status={} residue={} sig=0x{:08x}",
                csw.tag_val(),
                csw.status_val(),
                csw.data_residue_val(),
                csw.signature()
            );
            self.check_csw(csw, tag)?;
        } else if policy == CswPolicy::Required {
            return Err("CSW unexpectedly missing".to_string());
        }
        Ok(data)
    }

    fn write_bulk(&self, data: &[u8]) -> Result<(), String> {
        self.write_bulk_with_timeout(data, bulk_timeout())
    }

    fn write_bulk_with_timeout(&self, data: &[u8], timeout: Duration) -> Result<(), String> {
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + BULK_WRITE_CHUNK).min(data.len());
            let n = self
                .handle
                .write_bulk(BULK_OUT_EP, &data[written..end], timeout)
                .map_err(|e| format!("Bulk write failed at {}/{}: {}", written, data.len(), e))?;
            if n == 0 {
                return Err("Bulk write made no progress".to_string());
            }
            written += n;
        }
        Ok(())
    }

    fn write_bulk_data_phase(&self, payload: &[u8]) -> Result<(), String> {
        let deadline = Instant::now() + START_WRITE_RETRY_TIMEOUT;
        let mut attempts = 0usize;
        loop {
            match self.write_bulk_with_timeout(payload, Duration::from_secs(1)) {
                Ok(()) => return Ok(()),
                Err(e)
                    if is_bulk_write_start_timeout(&e, payload.len())
                        && Instant::now() < deadline =>
                {
                    attempts += 1;
                    log_verbose!(
                        "  >> DATA start retry #{} after endpoint settle: {}",
                        attempts,
                        e
                    );
                    thread::sleep(START_WRITE_RETRY_DELAY);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn read_bulk_to_buffer(&mut self, timeout: Duration) -> Result<usize, rusb::Error> {
        let mut buf = [0u8; 64 * 1024];
        let n = self.handle.read_bulk(BULK_IN_EP, &mut buf, timeout)?;
        if n > 0 {
            log_verbose!("  << IN EP raw {} bytes: {:02x?}", n, &buf[..n.min(128)]);
            self.in_buf.extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }

    fn read_exact_from_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + timeout;
        while self.in_buf.len() < len {
            let now = Instant::now();
            if now >= deadline {
                if let Some(err) = self.unexpected_csw_error("Bulk read timed out") {
                    return Err(err);
                }
                return Err(format!(
                    "Bulk read timed out with {}/{} bytes buffered",
                    self.in_buf.len(),
                    len
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            match self.read_bulk_to_buffer(remaining.min(bulk_timeout())) {
                Ok(0) => {}
                Ok(n) => log_verbose!(
                    "  << DATA buffered {} bytes (buffer={}/{})",
                    n,
                    self.in_buf.len(),
                    len
                ),
                Err(rusb::Error::Timeout) => {
                    if let Some(err) = self.unexpected_csw_error("Bulk read timed out") {
                        return Err(err);
                    }
                    return Err(format!(
                        "Bulk read timed out with {}/{} bytes buffered",
                        self.in_buf.len(),
                        len
                    ));
                }
                Err(e) => return Err(format!("Bulk read failed: {}", e)),
            }
        }
        Ok(self.in_buf.drain(..len).collect())
    }

    fn unexpected_csw_error(&self, context: &str) -> Option<String> {
        if self.in_buf.len() < 13 || self.find_csw_signature() != Some(0) {
            return None;
        }
        let csw = AicCsw::from_bytes(&self.in_buf[..13])?;
        Some(format!(
            "{}: device returned CSW instead of DATA (tag={}, status={}, residue={}, buffered={})",
            context,
            csw.tag_val(),
            csw.status_val(),
            csw.data_residue_val(),
            self.in_buf.len()
        ))
    }

    fn find_csw_signature(&self) -> Option<usize> {
        let sig = AIC_USB_SIGN_USBS.to_le_bytes();
        self.in_buf.windows(4).position(|w| w == sig)
    }

    fn read_csw(&mut self, expected_tag: u32, policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        let deadline = Instant::now()
            + if policy == CswPolicy::AllowMissing {
                SHORT_TIMEOUT
            } else {
                bulk_timeout()
            };
        loop {
            if let Some(pos) = self.find_csw_signature() {
                if pos > 0 {
                    log_verbose!("  << Dropping {} non-CSW stale bytes before USBS", pos);
                    self.in_buf.drain(..pos);
                }
                if self.in_buf.len() < 13 {
                    if let Err(e) = self.fill_until(deadline, 13) {
                        if policy == CswPolicy::AllowMissing {
                            log_verbose!(
                                "  << Incomplete CSW accepted by transaction policy: {}",
                                e
                            );
                            self.in_buf.clear();
                            return Ok(None);
                        }
                        return Err(e);
                    }
                    continue;
                }
                let csw = AicCsw::from_bytes(&self.in_buf[..13])
                    .ok_or_else(|| "Failed to parse CSW".to_string())?;
                log_verbose!(
                    "  << CSW candidate sig=0x{:08x} tag={} status={} residue={}",
                    csw.signature(),
                    csw.tag_val(),
                    csw.status_val(),
                    csw.data_residue_val()
                );
                self.in_buf.drain(..13);

                if csw.tag_val() == expected_tag {
                    return Ok(Some(csw));
                }

                log_verbose!(
                    "  << Discarding stale CSW tag={} while expecting tag={}",
                    csw.tag_val(),
                    expected_tag
                );
                continue;
            }

            if !self.in_buf.is_empty() && self.in_buf.len() > 3 {
                let keep = self.in_buf.split_off(self.in_buf.len() - 3);
                let dropped = std::mem::replace(&mut self.in_buf, keep).len();
                log_verbose!("  << Dropping {} bytes without CSW signature", dropped);
            }

            let now = Instant::now();
            if now >= deadline {
                if policy == CswPolicy::AllowMissing {
                    log_verbose!("  << No CSW before timeout; accepted by transaction policy");
                    return Ok(None);
                }
                return Err(format!("No CSW for tag {} before timeout", expected_tag));
            }
            match self
                .read_bulk_to_buffer(deadline.saturating_duration_since(now).min(SHORT_TIMEOUT))
            {
                Ok(_) => {}
                Err(rusb::Error::Timeout) if policy == CswPolicy::AllowMissing => {
                    log_verbose!("  << No CSW after short timeout; accepted by transaction policy");
                    return Ok(None);
                }
                Err(rusb::Error::Pipe | rusb::Error::NoDevice)
                    if policy == CswPolicy::AllowMissing =>
                {
                    log_verbose!(
                        "  << Device disconnected before CSW; accepted by transaction policy"
                    );
                    return Ok(None);
                }
                Err(rusb::Error::Timeout) => {}
                Err(e) => return Err(format!("IN EP read failed: {}", e)),
            }
        }
    }

    fn fill_until(&mut self, deadline: Instant, min_len: usize) -> Result<(), String> {
        while self.in_buf.len() < min_len {
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "Timed out waiting for {} buffered bytes (have {})",
                    min_len,
                    self.in_buf.len()
                ));
            }
            match self
                .read_bulk_to_buffer(deadline.saturating_duration_since(now).min(SHORT_TIMEOUT))
            {
                Ok(_) => {}
                Err(rusb::Error::Timeout) => {}
                Err(e) => return Err(format!("IN EP read failed: {}", e)),
            }
        }
        Ok(())
    }

    fn check_csw(&self, csw: &AicCsw, expected_tag: u32) -> Result<(), String> {
        if csw.tag_val() != expected_tag {
            return Err(format!(
                "CSW tag mismatch: got {}, expected {}",
                csw.tag_val(),
                expected_tag
            ));
        }
        if !csw.is_ok() {
            return Err(format!(
                "CSW failed: status={} residue={}",
                csw.status_val(),
                csw.data_residue_val()
            ));
        }
        Ok(())
    }
}

impl UpgTransport for UsbTransport {
    fn write_txn(&mut self, payload: &[u8], policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        match self.write_txn_inner(payload, policy) {
            Ok(csw) => Ok(csw),
            Err(e) if e.contains("Bulk write failed at 0/31") || e.contains("Pipe") => {
                log_verbose!("  >> WRITE retry after reconnect: {}", e);
                self.wait_reconnect(Duration::from_secs(10))?;
                self.write_txn_inner(payload, policy)
            }
            Err(e) => Err(e),
        }
    }

    fn read_txn(&mut self, read_len: u32, policy: CswPolicy) -> Result<Vec<u8>, String> {
        self.read_txn_inner(read_len, policy)
    }

    fn reconnect(&mut self, timeout: Duration) -> Result<(), String> {
        self.wait_reconnect(timeout)
    }

    fn drain_rx(&mut self, timeout: Duration, max_bytes: usize) -> Result<(), String> {
        self.drain_in_endpoint(timeout, max_bytes)
    }

    fn transport_name(&self) -> &'static str {
        "USB"
    }

    fn max_write_chunk(&self, _block_size: u32) -> usize {
        BULK_WRITE_CHUNK
    }
}

impl Drop for UsbTransport {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(0);
    }
}

fn is_bulk_write_start_timeout(err: &str, len: usize) -> bool {
    err.contains(&format!(
        "Bulk write failed at 0/{}: Operation timed out",
        len
    ))
}

fn scan_devices_libusb() -> Result<Vec<DeviceInfo>, String> {
    let context = rusb::Context::new().map_err(|e| format!("Failed to init USB: {}", e))?;
    let devices = context
        .devices()
        .map_err(|e| format!("Failed to list USB devices: {}", e))?;

    let mut found = Vec::new();
    for device in devices.iter() {
        let desc = device
            .device_descriptor()
            .map_err(|e| format!("Failed to get device descriptor: {}", e))?;
        if desc.vendor_id() != AIC_VID || desc.product_id() != AIC_PID {
            continue;
        }
        let port_path = device
            .port_numbers()
            .map(|ports| {
                ports
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join("-")
            })
            .unwrap_or_default();
        found.push(DeviceInfo {
            bus_number: device.bus_number(),
            address: device.address(),
            vendor_id: desc.vendor_id(),
            product_id: desc.product_id(),
            port_path,
            speed: format!("{:?}", device.speed()),
            ready: true,
            ioreg_ready: None,
            protocol_ready: None,
            status: None,
        });
    }
    Ok(found)
}

#[cfg(target_os = "macos")]
fn scan_devices_macos_ioreg() -> Result<Vec<DeviceInfo>, String> {
    let output = Command::new("ioreg")
        .args(["-r", "-c", "IOUSBHostDevice", "-l", "-w", "0", "-d", "2"])
        .output()
        .map_err(|e| format!("Failed to run ioreg: {}", e))?;
    if !output.status.success() {
        return Err(format!("ioreg exited with {}", output.status));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|e| format!("ioreg output is not UTF-8: {}", e))?;
    Ok(parse_ioreg_devices(&text))
}

#[cfg(target_os = "macos")]
fn apply_macos_diagnostics(devices: &mut [DeviceInfo]) {
    let Ok(statuses) = scan_devices_macos_ioreg() else {
        return;
    };
    merge_ioreg_diagnostics(devices, &statuses);
}

/// Merge IOKit diagnostics without touching libusb enumeration (`ready`) or
/// protocol readiness.  A missing configuration/interface is useful evidence,
/// but it must never block a real libusb open attempt.
#[cfg(any(target_os = "macos", test))]
fn merge_ioreg_diagnostics(devices: &mut [DeviceInfo], statuses: &[DeviceInfo]) {
    for device in devices {
        let by_path = statuses
            .iter()
            .find(|status| !device.port_path.is_empty() && status.port_path == device.port_path);
        let by_address = || {
            statuses.iter().find(|status| {
                status.address != 0
                    && status.address == device.address
                    && (status.bus_number == device.bus_number || status.bus_number == 0)
            })
        };
        let matching = by_path.or_else(by_address);
        if let Some(status) = matching {
            device.ioreg_ready = Some(status.ioreg_ready.unwrap_or(status.ready));
            if status.status.is_some() {
                device.status.clone_from(&status.status);
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default, Debug, PartialEq, Eq)]
struct IoregUsbDevice {
    vendor_id: Option<u16>,
    product_id: Option<u16>,
    address: Option<u8>,
    location_id: Option<u32>,
    usb_speed: Option<u8>,
    configured: bool,
    has_interface: bool,
}

/// Parse `ioreg -r -c IOUSBHostDevice -l -w 0 -d 2` output.
///
/// A device's property block closes before its child interface nodes appear,
/// so the parser keeps collecting the whole root subtree until the next
/// top-level device.  Class names are matched by suffix so both
/// `IOUSBHostInterface` and `AppleUSBHostInterface` count.
#[cfg(any(target_os = "macos", test))]
fn parse_ioreg_devices(text: &str) -> Vec<DeviceInfo> {
    let mut found = Vec::new();
    let mut current: Option<(IoregUsbDevice, String, usize)> = None;

    for line in text.lines() {
        let class = ioreg_class_name(line);
        let indent = ioreg_node_indent(line);

        if let Some(class) = class {
            if ioreg_is_device_class(class) {
                if let Some((_, _, start_indent)) = current.as_ref() {
                    if indent > *start_indent {
                        // A nested USB host device is not part of the root
                        // property block we are parsing.
                        continue;
                    }
                }
                if let Some((device, _, _)) = current.take() {
                    push_ioreg_device(&mut found, Some(device));
                }
                current = Some((
                    IoregUsbDevice::default(),
                    ioreg_device_property_prefix(line),
                    indent,
                ));
                continue;
            } else if ioreg_is_interface_class(class) {
                if let Some((device, _, _)) = current.as_mut() {
                    device.has_interface = true;
                }
                continue;
            }
        }

        let Some((device, property_prefix, start_indent)) = current.as_mut() else {
            continue;
        };
        if property_prefix.is_empty()
            || !ioreg_line_matches_property(line, property_prefix, *start_indent)
        {
            continue;
        }

        if let Some(value) = parse_ioreg_number(line, "idVendor") {
            device.vendor_id = Some(value as u16);
        } else if let Some(value) = parse_ioreg_number(line, "idProduct") {
            device.product_id = Some(value as u16);
        } else if let Some(value) = parse_ioreg_number(line, "kUSBCurrentConfiguration") {
            device.configured = value > 0;
        } else if let Some(value) = parse_ioreg_number(line, "kUSBAddress")
            .or_else(|| parse_ioreg_number(line, "USB Address"))
        {
            device.address = Some(value as u8);
        } else if let Some(value) = parse_ioreg_number(line, "locationID") {
            device.location_id = Some(value as u32);
        } else if let Some(value) = parse_ioreg_number(line, "USBSpeed") {
            device.usb_speed = Some(value as u8);
        }
    }
    if let Some((device, _, _)) = current {
        push_ioreg_device(&mut found, Some(device));
    }
    found
}

#[cfg(any(target_os = "macos", test))]
fn push_ioreg_device(found: &mut Vec<DeviceInfo>, device: Option<IoregUsbDevice>) {
    let Some(device) = device else {
        return;
    };
    if device.vendor_id != Some(AIC_VID) || device.product_id != Some(AIC_PID) {
        return;
    }
    let status = macos_not_ready_status(device.configured, device.has_interface);
    found.push(DeviceInfo {
        bus_number: 0,
        address: device.address.unwrap_or(0),
        vendor_id: AIC_VID,
        product_id: AIC_PID,
        port_path: device
            .location_id
            .map(location_id_to_port_path)
            .unwrap_or_default(),
        speed: device
            .usb_speed
            .map(usb_speed_label)
            .unwrap_or("Unknown")
            .to_string(),
        // IOKit found the device, so it is selectable, but this is not
        // protocol readiness and it must not gate a later libusb open.
        ready: true,
        ioreg_ready: Some(status.is_none()),
        protocol_ready: None,
        status,
    });
}

#[cfg(any(target_os = "macos", test))]
fn ioreg_node_indent(line: &str) -> usize {
    line.split_once("+-o ")
        .map(|(indent, _)| indent.len())
        .unwrap_or(usize::MAX)
}

#[cfg(any(target_os = "macos", test))]
fn ioreg_class_name(line: &str) -> Option<&str> {
    let rest = line.split_once("<class ")?.1;
    let end = rest
        .find(|ch: char| ch == ',' || ch == '>' || ch.is_whitespace())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

#[cfg(any(target_os = "macos", test))]
fn ioreg_is_device_class(class: &str) -> bool {
    class.ends_with("USBHostDevice")
}

#[cfg(any(target_os = "macos", test))]
fn ioreg_is_interface_class(class: &str) -> bool {
    class.ends_with("USBHostInterface")
}

#[cfg(any(target_os = "macos", test))]
fn ioreg_device_property_prefix(device_line: &str) -> String {
    let indent = device_line.split("+-o ").next().unwrap_or_default();
    format!("{}  |", indent)
}

#[cfg(any(target_os = "macos", test))]
fn ioreg_line_matches_property(line: &str, bar_prefix: &str, indent: usize) -> bool {
    if line.starts_with(bar_prefix) {
        return true;
    }
    // ioreg omits the "|" column when a node has no following siblings.
    let plain_prefix = format!("{}{}", " ".repeat(indent), "    ");
    line.starts_with(&plain_prefix)
}

#[cfg(any(target_os = "macos", test))]
fn parse_ioreg_number(line: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{}\" = ", key);
    let value = line.split_once(&pattern)?.1.trim();
    value
        .split(|ch: char| !ch.is_ascii_hexdigit() && ch != 'x' && ch != 'X')
        .next()
        .and_then(|raw| {
            if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).ok()
            } else {
                raw.parse().ok()
            }
        })
}

#[cfg(any(target_os = "macos", test))]
fn location_id_to_port_path(location_id: u32) -> String {
    let path = location_id >> 16;
    let text = format!("{:04x}", path);
    let ports = text.trim_start_matches('0');
    if ports.is_empty() {
        String::new()
    } else {
        ports
            .chars()
            .map(|ch| ch.to_string())
            .collect::<Vec<_>>()
            .join("-")
    }
}

#[cfg(any(target_os = "macos", test))]
fn usb_speed_label(speed: u8) -> &'static str {
    match speed {
        1 => "Low/Full",
        2 => "Full",
        3 => "High",
        4 => "Super",
        _ => "Unknown",
    }
}

#[cfg(target_os = "macos")]
fn macos_ioreg_diagnostic(address: u8) -> Option<String> {
    let devices = scan_devices_macos_ioreg().ok()?;
    devices
        .iter()
        .find(|device| device.address == address)
        .and_then(|device| device.status.clone())
}

#[cfg(any(target_os = "macos", test))]
fn macos_not_ready_status(configured: bool, has_interface: bool) -> Option<String> {
    if configured && has_interface {
        return None;
    }

    let mut reasons = Vec::new();
    if !configured {
        reasons.push("missing kUSBCurrentConfiguration");
    }
    if !has_interface {
        reasons.push("no USBHostInterface child");
    }
    Some(format!(
        "IOKit reports: {}. This is diagnostic only and does not block libusb access. Reconnect the board in upgrade mode, preferably directly to the Mac without a hub; if it persists, power-cycle the board or reboot macOS.",
        reasons.join(", ")
    ))
}

#[derive(Clone)]
struct UsbAccessLock {
    _file: Arc<File>,
}

impl UsbAccessLock {
    fn try_acquire() -> Result<Self, String> {
        let path = usb_access_lock_path()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("Failed to open USB access lock '{}': {}", path.display(), e))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = file.set_len(0);
                let mut file_for_pid = file
                    .try_clone()
                    .map_err(|e| format!("Failed to clone USB access lock: {}", e))?;
                let _ = writeln!(file_for_pid, "pid={}", std::process::id());
                Ok(Self {
                    _file: Arc::new(file),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(format!(
                "Another artinchip-flash instance is using the ArtInChip USB device{} Close the other CLI/GUI instance and retry. Lock: {}",
                lock_owner_hint(&path),
                path.display()
            )),
            Err(e) => Err(format!(
                "Failed to lock ArtInChip USB access '{}': {}",
                path.display(),
                e
            )),
        }
    }
}

fn lock_owner_hint(path: &std::path::Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(owner) if !owner.trim().is_empty() => format!(" ({}).", owner.trim()),
        _ => ".".to_string(),
    }
}

fn usb_access_lock_path() -> Result<PathBuf, String> {
    let dir = platform_config_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create '{}': {}", dir.display(), e))?;
    Ok(dir.join("artinchip-flash-usb-33c3-6677.lock"))
}

fn platform_config_dir() -> PathBuf {
    crate::paths::config_dir("artinchip-flash")
}

fn format_usb_error(err: &rusb::Error) -> String {
    format!(
        "{} (rusb::Error::{:?}, libusb code {})",
        err,
        err,
        libusb_error_code(err)
    )
}

fn libusb_error_code(err: &rusb::Error) -> i32 {
    match err {
        rusb::Error::Io => -1,
        rusb::Error::InvalidParam => -2,
        rusb::Error::Access => -3,
        rusb::Error::NoDevice => -4,
        rusb::Error::NotFound => -5,
        rusb::Error::Busy => -6,
        rusb::Error::Timeout => -7,
        rusb::Error::Overflow => -8,
        rusb::Error::Pipe => -9,
        rusb::Error::Interrupted => -10,
        rusb::Error::NoMem => -11,
        rusb::Error::NotSupported => -12,
        rusb::Error::BadDescriptor | rusb::Error::Other => -99,
    }
}

#[cfg(target_os = "macos")]
fn format_usb_open_error(err: &rusb::Error, address: u8) -> String {
    let mut text = format_usb_error(err);
    #[cfg(target_os = "macos")]
    {
        if let Some(reason) = macos_ioreg_diagnostic(address) {
            text.push_str(&format!("; IOKit state: {}", reason));
        }
        if *err == rusb::Error::Other {
            text.push_str(
                "; macOS IOKit may have refused USBDeviceOpen. Close other artinchip-flash/AiBurn instances, then unplug and reconnect the board",
            );
        }
    }
    text
}

#[cfg(not(target_os = "macos"))]
fn format_usb_open_error(err: &rusb::Error, _address: u8) -> String {
    format_usb_error(err)
}

fn validate_required_endpoint_addresses(addresses: &[u8]) -> Result<(), String> {
    let missing = [BULK_OUT_EP, BULK_IN_EP]
        .into_iter()
        .filter(|required| !addresses.contains(required))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "configuration descriptor is missing required bulk endpoint(s): {}",
        missing
            .iter()
            .map(|address| format!("0x{address:02x}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn validate_configuration_endpoints(config: &rusb::ConfigDescriptor) -> Result<(), String> {
    let mut interface_found = false;
    let mut endpoints = Vec::new();
    for interface in config.interfaces() {
        for setting in interface.descriptors() {
            if setting.interface_number() != 0 {
                continue;
            }
            interface_found = true;
            for endpoint in setting.endpoint_descriptors() {
                endpoints.push(endpoint.address());
            }
        }
    }
    if !interface_found {
        return Err(
            "configuration descriptor has no interface 0; this is a device-side USB descriptor/configuration problem"
                .to_string(),
        );
    }
    validate_required_endpoint_addresses(&endpoints)
}

fn log_configuration_descriptor(config: &rusb::ConfigDescriptor) {
    log_verbose!(
        "  Configuration {}: {} interface(s)",
        config.number(),
        config.num_interfaces()
    );
    for interface in config.interfaces() {
        for setting in interface.descriptors() {
            log_verbose!(
                "  Interface {} alt {}: {} endpoints",
                setting.interface_number(),
                setting.setting_number(),
                setting.num_endpoints()
            );
            for endpoint in setting.endpoint_descriptors() {
                log_verbose!(
                    "    EP 0x{:02x} {} {:?} max_packet={}",
                    endpoint.address(),
                    if endpoint.direction() == rusb::Direction::In {
                        "IN"
                    } else {
                        "OUT"
                    },
                    endpoint.transfer_type(),
                    endpoint.max_packet_size()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_override_parses_and_floors() {
        assert_eq!(parse_timeout_ms(None), TIMEOUT_MS);
        assert_eq!(parse_timeout_ms(Some("5000")), Duration::from_millis(5000));
        assert_eq!(
            parse_timeout_ms(Some(" 2000 ")),
            Duration::from_millis(2000)
        );
        // Floor, garbage and empty fall back (never fail the transfer path).
        assert_eq!(parse_timeout_ms(Some("0")), Duration::from_millis(500));
        assert_eq!(parse_timeout_ms(Some("100")), Duration::from_millis(500));
        assert_eq!(parse_timeout_ms(Some("nope")), TIMEOUT_MS);
        assert_eq!(parse_timeout_ms(Some("")), TIMEOUT_MS);
    }

    #[test]
    fn selector_parses_port_path_and_bus_address() {
        assert_eq!(DeviceSelector::parse(None).unwrap(), DeviceSelector::Unique);
        assert_eq!(
            DeviceSelector::parse(Some("  ")).unwrap(),
            DeviceSelector::Unique
        );
        assert_eq!(
            DeviceSelector::parse(Some("9-2")).unwrap(),
            DeviceSelector::PortPath("9-2".to_string())
        );
        assert_eq!(
            DeviceSelector::parse(Some("1:45")).unwrap(),
            DeviceSelector::BusAddress(1, 45)
        );
        assert!(DeviceSelector::parse(Some("1:999")).is_err());
        assert!(DeviceSelector::parse(Some("x:y")).is_err());
    }

    #[test]
    fn unique_selector_rejects_multiple_candidates() {
        let candidates = vec![(1, 10, "9-2".to_string()), (1, 11, "9-3".to_string())];
        let err = select_candidate(&DeviceSelector::Unique, &candidates).unwrap_err();
        assert!(err.contains("Multiple ArtInChip devices"), "{}", err);
        assert!(err.contains("9-2"), "{}", err);

        let one = vec![(1, 10, "9-2".to_string())];
        assert_eq!(
            select_candidate(&DeviceSelector::Unique, &one).unwrap(),
            (1, 10)
        );
    }

    #[test]
    fn a_volatile_selection_pins_to_one_stable_port() {
        let candidates = [(1, 15, "9-2".into()), (1, 16, "9-3".into())];
        assert!(pin_candidate(&DeviceSelector::Unique, &candidates).is_err());
        assert_eq!(
            pin_candidate(&DeviceSelector::BusAddress(1, 15), &candidates).unwrap(),
            DeviceSelector::PhysicalPort(1, "9-2".into()),
        );
        assert!(pin_candidate(
            &DeviceSelector::BusAddress(1, 15),
            &[(1, 15, String::new())]
        )
        .is_err());
    }

    #[test]
    fn selector_picks_port_path_and_rejects_missing() {
        let candidates = vec![(1, 10, "9-2".to_string()), (1, 11, "9-3".to_string())];
        assert_eq!(
            select_candidate(&DeviceSelector::PortPath("9-3".to_string()), &candidates).unwrap(),
            (1, 11)
        );
        assert!(
            select_candidate(&DeviceSelector::PortPath("9-9".to_string()), &candidates).is_err()
        );
        assert_eq!(
            select_candidate(&DeviceSelector::BusAddress(1, 10), &candidates).unwrap(),
            (1, 10)
        );
        assert!(select_candidate(&DeviceSelector::BusAddress(1, 99), &candidates).is_err());
    }

    #[test]
    fn physical_selector_follows_address_changes_without_following_another_bus_or_port() {
        let selector = DeviceSelector::PhysicalPort(1, "9-2".to_owned());
        let before = [(1, 10, "9-2".to_owned())];
        let after = [
            (1, 10, "9-3".to_owned()),
            (2, 21, "9-2".to_owned()),
            (1, 22, "9-2".to_owned()),
        ];
        assert_eq!(select_candidate(&selector, &before), Ok((1, 10)));
        assert_eq!(select_candidate(&selector, &after), Ok((1, 22)));
        assert!(select_candidate(&selector, &after[..2]).is_err());
    }

    #[test]
    fn parses_macos_27_unconfigured_aic_device_without_blocking_enumeration() {
        let text = r#"
+-o IOUSBHostDevice@00110000  <class IOUSBHostDevice, id 0x100003fb6, registered, matched, active, busy 0 (5073 ms), retain 15>
  | {
  |   "USBSpeed" = 3
  |   "UsbLinkSpeed" = 480000000
  |   "idProduct" = 26231
  |   "bDeviceClass" = 0
  |   "bNumConfigurations" = 1
  |   "USB Address" = 7
  |   "locationID" = 1114112
  |   "Device Speed" = 2
  |   "idVendor" = 13251
  |   "kUSBAddress" = 7
  | }
  |
  +-o AppleUSBHostCompositeDevice  <class AppleUSBHostCompositeDevice, id 0x100003fd3, !registered, !matched, active, busy 0, retain 5>
      {
        "IOProviderClass" = "IOUSBHostDevice"
        "bDeviceClass" = 0
      }
"#;
        let devices = parse_ioreg_devices(text);
        assert_eq!(devices.len(), 1);
        let device = &devices[0];
        assert_eq!(device.bus_number, 0);
        assert_eq!(device.address, 7);
        assert_eq!(device.port_path, "1-1");
        assert_eq!(device.speed, "High");
        assert!(
            device.ready,
            "IOKit detection must keep the device selectable"
        );
        assert_eq!(device.ioreg_ready, Some(false));
        assert_eq!(device.protocol_ready, None);
        let status = device.status.as_deref().unwrap();
        assert!(status.contains("kUSBCurrentConfiguration"), "{status}");
        assert!(status.contains("USBHostInterface"), "{status}");
        assert!(status.contains("does not block libusb"), "{status}");
    }

    #[test]
    fn parses_interface_child_after_the_device_property_block() {
        let text = r#"
+-o IOUSBHostDevice@00120000  <class IOUSBHostDevice, id 0x100003f03, registered, matched, active, busy 0 (59 ms), retain 37>
  | {
  |   "USBSpeed" = 3
  |   "idProduct" = 26231
  |   "USB Address" = 4
  |   "locationID" = 1179648
  |   "kUSBCurrentConfiguration" = 1
  |   "idVendor" = 13251
  |   "kUSBAddress" = 4
  | }
  |
  +-o AppleUSBHostInterface@0  <class AppleUSBHostInterface, id 0x100003f09, registered, matched, active, busy 0 (37 ms), retain 12>
      {
        "bInterfaceNumber" = 0
        "bNumEndpoints" = 2
      }
"#;
        let devices = parse_ioreg_devices(text);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].ioreg_ready, Some(true));
        assert_eq!(devices[0].status, None);
    }

    #[test]
    fn ioreg_zero_configuration_is_not_configured_and_class_suffixes_are_supported() {
        let text = r#"
+-o AppleUSBHostDevice@00300000  <class AppleUSBHostDevice, id 0x42, registered, matched, active, busy 0, retain 3>
  | {
  |   "idProduct" = 26231
  |   "kUSBCurrentConfiguration" = 0
  |   "idVendor" = 13251
  | }
  |
  +-o IOUSBHostInterface@0  <class IOUSBHostInterface, id 0x43, registered, matched, active, busy 0, retain 3>
      {
        "bInterfaceNumber" = 0
      }
"#;
        let devices = parse_ioreg_devices(text);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].ioreg_ready, Some(false));
        assert!(devices[0]
            .status
            .as_deref()
            .is_some_and(|status| status.contains("kUSBCurrentConfiguration")));
    }

    #[test]
    fn parses_ioreg_output_without_vertical_bar_columns() {
        let text = r#"
+-o IOUSBHostDevice@00200000  <class IOUSBHostDevice, id 0x55, registered, matched, active, busy 0, retain 2>
  {
    "USBSpeed" = 3
    "idProduct" = 26231
    "USB Address" = 8
    "locationID" = 1114112
    "kUSBCurrentConfiguration" = 1
    "idVendor" = 13251
  }
  +-o IOUSBHostInterface@0  <class IOUSBHostInterface, id 0x56, registered, matched, active, busy 0, retain 2>
      {
        "bInterfaceNumber" = 0
      }
"#;
        let devices = parse_ioreg_devices(text);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].address, 8);
        assert_eq!(devices[0].port_path, "1-1");
        assert_eq!(devices[0].ioreg_ready, Some(true));
        assert_eq!(devices[0].status, None);
    }

    #[test]
    fn ioreg_device_blocks_end_at_the_next_root_device() {
        let text = r#"
+-o IOUSBHostDevice@00110000  <class IOUSBHostDevice, id 0x1, registered, matched, active, busy 0, retain 1>
  | {
  |   "idProduct" = 26231
  |   "USB Address" = 7
  |   "kUSBCurrentConfiguration" = 1
  |   "idVendor" = 13251
  | }
  |
  +-o IOUSBHostInterface@0  <class IOUSBHostInterface, id 0x2, registered, matched, active, busy 0, retain 1>
      {
        "bInterfaceNumber" = 0
      }
+-o IOUSBHostDevice@00120000  <class IOUSBHostDevice, id 0x3, registered, matched, active, busy 0, retain 1>
  | {
  |   "idProduct" = 26231
  |   "USB Address" = 8
  |   "idVendor" = 13251
  | }
"#;
        let devices = parse_ioreg_devices(text);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].address, 7);
        assert_eq!(devices[0].ioreg_ready, Some(true));
        assert_eq!(devices[1].address, 8);
        assert_eq!(devices[1].ioreg_ready, Some(false));
    }

    #[test]
    fn ioreg_diagnostics_do_not_override_libusb_enumeration_or_protocol_state() {
        let ioreg = parse_ioreg_devices(
            r#"
+-o IOUSBHostDevice@00110000  <class IOUSBHostDevice, id 0x1, registered, matched, active, busy 0, retain 1>
  | {
  |   "idProduct" = 26231
  |   "USB Address" = 7
  |   "locationID" = 1114112
  |   "idVendor" = 13251
  | }
"#,
        );
        let mut devices = vec![DeviceInfo {
            bus_number: 0,
            address: 7,
            vendor_id: AIC_VID,
            product_id: AIC_PID,
            port_path: "1-1".to_string(),
            speed: "High".to_string(),
            ready: true,
            ioreg_ready: None,
            protocol_ready: None,
            status: None,
        }];
        merge_ioreg_diagnostics(&mut devices, &ioreg);
        assert!(devices[0].ready);
        assert_eq!(devices[0].ioreg_ready, Some(false));
        assert_eq!(devices[0].protocol_ready, None);
        assert!(devices[0].status.is_some());
    }

    #[test]
    fn usb_errors_keep_the_raw_rusb_variant_and_libusb_code() {
        let text = format_usb_error(&rusb::Error::Other);
        assert!(text.contains("Other error"), "{text}");
        assert!(text.contains("rusb::Error::Other"), "{text}");
        assert!(text.contains("libusb code -99"), "{text}");
        assert!(format_usb_error(&rusb::Error::Busy).contains("libusb code -6"));
    }

    #[test]
    fn required_bulk_endpoints_are_validated_explicitly() {
        assert!(validate_required_endpoint_addresses(&[BULK_OUT_EP, BULK_IN_EP]).is_ok());
        assert!(validate_required_endpoint_addresses(&[0x02, 0x81, 0x83]).is_ok());
        let error = validate_required_endpoint_addresses(&[0x81]).unwrap_err();
        assert!(error.contains("0x02"), "{error}");
        let error = validate_required_endpoint_addresses(&[0x02]).unwrap_err();
        assert!(error.contains("0x81"), "{error}");
    }
}
