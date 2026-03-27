// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # rtlsdr Library
//! Library for interfacing with an RTL-SDR device.

mod device;
pub mod error;
mod rtlsdr;
mod tuners;

use device::{Device, SharedReaderHandle};
use error::{Result, RtlsdrError};
use rtlsdr::RtlSdr as Sdr;
use rusb::{Context, DeviceHandle, DeviceList, UsbContext};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tuners::r82xx::{R820T_TUNER_ID, R828D_TUNER_ID};

pub struct TunerId;
impl TunerId {
    pub const R820T: &'static str = R820T_TUNER_ID;
    pub const R828D: &'static str = R828D_TUNER_ID;
}

pub const DEFAULT_BUF_LENGTH: usize = 16 * 16384;
pub const DEFAULT_ASYNC_BUF_NUMBER: usize = 15;

pub struct AsyncReadHandle {
    rx: mpsc::Receiver<Result<Vec<u8>>>,
    ctrl_tx: mpsc::Sender<AsyncReadControl>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct AsyncReadControlHandle {
    ctrl_tx: mpsc::Sender<AsyncReadControl>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

enum AsyncReadControl {
    Tune(u32),
    SetGain(TunerGain),
    SetSampleRate(u32),
}

impl AsyncReadHandle {
    pub fn control_handle(&self) -> AsyncReadControlHandle {
        AsyncReadControlHandle {
            ctrl_tx: self.ctrl_tx.clone(),
            stop: self.stop.clone(),
            dropped: self.dropped.clone(),
        }
    }

    pub fn recv(&self) -> Option<Result<Vec<u8>>> {
        self.rx.recv().ok()
    }

    pub fn try_recv(&self) -> Option<Result<Vec<u8>>> {
        self.rx.try_recv().ok()
    }

    pub fn dropped_chunks(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn tune(&self, center_freq: u32) -> Result<()> {
        self.ctrl_tx
            .send(AsyncReadControl::Tune(center_freq))
            .map_err(|_| RtlsdrError::RtlsdrErr("async control channel closed".to_string()))
    }

    pub fn set_gain(&self, gain: TunerGain) -> Result<()> {
        self.ctrl_tx
            .send(AsyncReadControl::SetGain(gain))
            .map_err(|_| RtlsdrError::RtlsdrErr("async control channel closed".to_string()))
    }

    pub fn set_sample_rate(&self, rate: u32) -> Result<()> {
        self.ctrl_tx
            .send(AsyncReadControl::SetSampleRate(rate))
            .map_err(|_| RtlsdrError::RtlsdrErr("async control channel closed".to_string()))
    }
}

impl AsyncReadControlHandle {
    pub fn dropped_chunks(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn tune(&self, center_freq: u32) -> Result<()> {
        self.ctrl_tx
            .send(AsyncReadControl::Tune(center_freq))
            .map_err(|_| RtlsdrError::RtlsdrErr("async control channel closed".to_string()))
    }

    pub fn set_gain(&self, gain: TunerGain) -> Result<()> {
        self.ctrl_tx
            .send(AsyncReadControl::SetGain(gain))
            .map_err(|_| RtlsdrError::RtlsdrErr("async control channel closed".to_string()))
    }

    pub fn set_sample_rate(&self, rate: u32) -> Result<()> {
        self.ctrl_tx
            .send(AsyncReadControl::SetSampleRate(rate))
            .map_err(|_| RtlsdrError::RtlsdrErr("async control channel closed".to_string()))
    }
}

impl Iterator for AsyncReadHandle {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

impl Drop for AsyncReadHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

pub struct DeviceDescriptors {
    list: DeviceList<Context>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub index: usize,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
}

impl DeviceDescriptors {
    pub fn new() -> Result<Self> {
        let context = Context::new()?;
        let list = context.devices()?;
        Ok(Self { list })
    }

    /// Returns an iterator over the found RTL-SDR devices.
    pub fn iter(&self) -> impl Iterator<Item = DeviceDescriptor> + '_ {
        self.list
            .iter()
            .filter_map(|device| {
                let desc = device.device_descriptor().ok()?;
                device::is_known_device(desc.vendor_id(), desc.product_id()).then_some(device)
            })
            .enumerate()
            .filter_map(|(index, device)| {
                let desc = device.device_descriptor().ok()?;
                match device.open() {
                    Ok(handle) => {
                        let manufacturer = read_string(&handle, desc.manufacturer_string_index());
                        let product = read_string(&handle, desc.product_string_index());
                        let serial = read_string(&handle, desc.serial_number_string_index());

                        Some(DeviceDescriptor {
                            index,
                            vendor_id: desc.vendor_id(),
                            product_id: desc.product_id(),
                            manufacturer,
                            product,
                            serial,
                        })
                    }
                    Err(e) => {
                        log::warn!("Could not open device at index {}: {}", index, e);
                        None
                    }
                }
            })
    }
}

fn read_string<T: UsbContext>(handle: &DeviceHandle<T>, index: Option<u8>) -> String {
    index
        .and_then(|i| handle.read_string_descriptor_ascii(i).ok())
        .unwrap_or_default()
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum DeviceId<'a> {
    Index(usize),
    Serial(&'a str),
    Fd(i32),
}

#[derive(Debug)]
pub enum TunerGain {
    Auto,
    Manual(i32),
}
#[derive(Debug)]
pub enum DirectSampleMode {
    Off,
    On,
    OnSwap, // Swap I and Q ADC, allowing to select between two inputs
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Sensor {
    TunerType,
    TunerGainDb,
    FrequencyCorrectionPpm,
}

#[derive(Debug, PartialEq)]
pub enum SensorValue {
    TunerType(String),
    TunerGainDb(i32),
    FrequencyCorrectionPpm(i32),
}

pub struct RtlSdr {
    sdr: Sdr,
}
impl RtlSdr {
    pub fn open(device_id: DeviceId) -> Result<RtlSdr> {
        let dev = Device::new(device_id)?;
        let mut sdr = Sdr::new(dev);
        sdr.init()?;
        Ok(RtlSdr { sdr })
    }

    pub fn open_with_serial(serial: &str) -> Result<RtlSdr> {
        Self::open(DeviceId::Serial(serial))
    }

    /// Convenience function to open device by index (backward compatibility)
    pub fn open_with_index(index: usize) -> Result<RtlSdr> {
        Self::open(DeviceId::Index(index))
    }

    /// Convenience function to open device by file descriptor  
    pub fn open_with_fd(fd: i32) -> Result<RtlSdr> {
        Self::open(DeviceId::Fd(fd))
    }
    pub fn close(&mut self) -> Result<()> {
        // TODO: wait until async is inactive
        self.sdr.deinit_baseband()
    }
    pub fn reset_buffer(&self) -> Result<()> {
        self.sdr.reset_buffer()
    }
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize> {
        self.sdr.read_sync(buf)
    }

    /// Start a callback-like async reader loop in a dedicated thread.
    ///
    /// This is a Rust-friendly equivalent of librtlsdr's async API shape:
    /// data is produced continuously into a bounded queue until `stop()` or drop.
    pub fn into_async_reader(self, buf_num: usize, buf_len: usize) -> Result<AsyncReadHandle> {
        let queue_len = if buf_num == 0 {
            DEFAULT_ASYNC_BUF_NUMBER
        } else {
            buf_num
        };
        let read_len = if buf_len == 0 {
            DEFAULT_BUF_LENGTH
        } else {
            buf_len
        };

        if !read_len.is_multiple_of(512) {
            return Err(RtlsdrError::RtlsdrErr(format!(
                "Invalid async buffer length {} (must be multiple of 512)",
                read_len
            )));
        }

        let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>>>(queue_len);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<AsyncReadControl>();
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let stop_thread = stop.clone();

        let thread = thread::spawn(move || {
            let mut sdr = self;
            let mut buf = vec![0u8; read_len];
            while !stop_thread.load(Ordering::Relaxed) {
                while let Ok(cmd) = ctrl_rx.try_recv() {
                    match cmd {
                        AsyncReadControl::Tune(center_freq) => {
                            if let Err(e) = sdr.set_center_freq(center_freq) {
                                let _ = tx.try_send(Err(e));
                                return;
                            }
                        }
                        AsyncReadControl::SetGain(gain) => {
                            if let Err(e) = sdr.set_tuner_gain(gain) {
                                let _ = tx.try_send(Err(e));
                                return;
                            }
                        }
                        AsyncReadControl::SetSampleRate(rate) => {
                            if let Err(e) = sdr.set_sample_rate(rate) {
                                let _ = tx.try_send(Err(e));
                                return;
                            }
                        }
                    }
                }

                match sdr.read_sync(&mut buf) {
                    Ok(n) => {
                        if n == 0 {
                            continue;
                        }
                        let chunk = buf[..n].to_vec();
                        if tx.send(Ok(chunk)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
            let _ = sdr.close();
        });

        Ok(AsyncReadHandle {
            rx,
            ctrl_tx,
            stop,
            dropped,
            thread: Some(thread),
        })
    }

    /// Start a concurrent multi-transfer USB streaming loop.
    ///
    /// Spawns `buf_num` reader threads (default 15), each blocking on
    /// `rusb::DeviceHandle::read_bulk` simultaneously.  Because all threads
    /// share the same underlying USB handle via `Arc`, the OS/libusb sees
    /// `buf_num` bulk transfers in-flight at once — the same concurrency
    /// guarantee as librtlsdr's `rtlsdr_read_async` with N=`buf_num`
    /// transfers, achieved with pure safe Rust and no raw FFI.
    ///
    /// Tune commands sent via the returned handle's `tune()` method are
    /// processed by a dedicated control thread that calls `set_center_freq`
    /// without pausing the reader threads.
    ///
    /// Returns the same `AsyncReadHandle` iterator as `into_async_reader`.
    pub fn into_multi_transfer_reader(
        self,
        buf_num: usize,
        buf_len: usize,
    ) -> Result<AsyncReadHandle> {
        let buf_num = if buf_num == 0 {
            DEFAULT_ASYNC_BUF_NUMBER
        } else {
            buf_num
        };
        let buf_len = if buf_len == 0 {
            DEFAULT_BUF_LENGTH
        } else {
            buf_len
        };

        if buf_len % 512 != 0 {
            return Err(RtlsdrError::RtlsdrErr(format!(
                "Invalid async buffer length {} (must be multiple of 512)",
                buf_len
            )));
        }

        // Channel: buf_num * 4 slots give the consumer headroom.  When full,
        // reader threads drop chunks rather than blocking (which would stall
        // that thread's resubmission and create the very FIFO gap we prevent).
        let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>>>(buf_num * 4);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<AsyncReadControl>();
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_handle = Arc::clone(&dropped);

        // Shared handle — Arc lets N threads call read_bulk concurrently.
        // rusb::DeviceHandle implements Send + Sync (libusb is thread-safe for
        // concurrent bulk transfers on the same handle).
        let shared = self.sdr.shared_reader_handle();

        // The full RtlSdr moves into the control thread for exclusive tuning.
        let mut sdr = self;

        let stop_ctrl = stop.clone();
        let thread = thread::spawn(move || {
            // Spawn N reader threads.  Each blocks on read_bulk with a short
            // timeout so it can notice the stop flag promptly on shutdown.
            let reader_threads: Vec<_> = (0..buf_num)
                .map(|_| {
                    let handle: SharedReaderHandle = shared.clone();
                    let tx = tx.clone();
                    let stop_r = stop_ctrl.clone();
                    let dropped_ctr = dropped.clone();
                    thread::spawn(move || {
                        let mut buf = vec![0u8; buf_len];
                        loop {
                            if stop_r.load(Ordering::Relaxed) {
                                break;
                            }
                            match handle.read_bulk(&mut buf, Duration::from_millis(100)) {
                                Ok(0) => continue,
                                Ok(n) => {
                                    if tx.try_send(Ok(buf[..n].to_vec())).is_err() {
                                        dropped_ctr.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(RtlsdrError::Usb(rusb::Error::Timeout)) => continue,
                                Err(RtlsdrError::Usb(rusb::Error::Interrupted)) => continue,
                                Err(e) => {
                                    let _ = tx.try_send(Err(e));
                                    break;
                                }
                            }
                        }
                    })
                })
                .collect();

            // Control loop: handle tune commands while readers run.
            loop {
                if stop_ctrl.load(Ordering::Relaxed) {
                    break;
                }
                match ctrl_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(AsyncReadControl::Tune(freq)) => {
                        if let Err(e) = sdr.sdr.set_center_freq(freq) {
                            log::warn!("RTL-SDR retune to {} Hz failed: {}", freq, e);
                        }
                    }
                    Ok(AsyncReadControl::SetGain(gain)) => {
                        if let Err(e) = sdr.set_tuner_gain(gain) {
                            log::warn!("RTL-SDR set gain failed: {}", e);
                        }
                    }
                    Ok(AsyncReadControl::SetSampleRate(rate)) => {
                        if let Err(e) = sdr.set_sample_rate(rate) {
                            log::warn!("RTL-SDR set sample rate to {} failed: {}", rate, e);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }

            // Shutdown: signal readers and wait for all to exit.
            stop_ctrl.store(true, Ordering::Relaxed);
            for t in reader_threads {
                let _ = t.join();
            }

            let _ = sdr.close();
        });

        Ok(AsyncReadHandle {
            rx,
            ctrl_tx,
            stop,
            dropped: dropped_handle,
            thread: Some(thread),
        })
    }

    pub fn get_center_freq(&self) -> u32 {
        self.sdr.get_center_freq()
    }
    pub fn set_center_freq(&mut self, freq: u32) -> Result<()> {
        self.sdr.set_center_freq(freq)
    }
    pub fn get_tuner_gains(&self) -> Result<Vec<i32>> {
        self.sdr.get_tuner_gains()
    }
    pub fn read_tuner_gain(&self) -> Result<i32> {
        self.sdr.read_tuner_gain()
    }
    pub fn set_tuner_gain(&mut self, gain: TunerGain) -> Result<()> {
        self.sdr.set_tuner_gain(gain)
    }
    pub fn get_freq_correction(&self) -> i32 {
        self.sdr.get_freq_correction()
    }
    pub fn set_freq_correction(&mut self, ppm: i32) -> Result<()> {
        self.sdr.set_freq_correction(ppm)
    }
    pub fn get_sample_rate(&self) -> u32 {
        self.sdr.get_sample_rate()
    }
    pub fn set_sample_rate(&mut self, rate: u32) -> Result<()> {
        self.sdr.set_sample_rate(rate)
    }
    pub fn set_tuner_bandwidth(&mut self, bw: u32) -> Result<()> {
        self.sdr.set_tuner_bandwidth(bw)
    }
    pub fn set_testmode(&mut self, on: bool) -> Result<()> {
        self.sdr.set_testmode(on)
    }
    pub fn set_direct_sampling(&mut self, mode: DirectSampleMode) -> Result<()> {
        self.sdr.set_direct_sampling(mode)
    }
    pub fn set_bias_tee(&self, on: bool) -> Result<()> {
        self.sdr.set_bias_tee(on)
    }
    pub fn get_tuner_id(&self) -> Result<&str> {
        self.sdr.get_tuner_id()
    }
    pub fn list_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(vec![
            Sensor::TunerType,
            Sensor::TunerGainDb,
            Sensor::FrequencyCorrectionPpm,
        ])
    }
    pub fn read_sensor(&self, sensor: Sensor) -> Result<SensorValue> {
        match sensor {
            Sensor::TunerType => self
                .get_tuner_id()
                .map(|s| SensorValue::TunerType(s.to_string())),
            Sensor::TunerGainDb => self.sdr.read_tuner_gain().map(SensorValue::TunerGainDb),
            Sensor::FrequencyCorrectionPpm => Ok(SensorValue::FrequencyCorrectionPpm(
                self.get_freq_correction(),
            )),
        }
    }

    /// Get the number of available RTL-SDR devices
    pub fn get_device_count() -> Result<usize> {
        let descriptors = DeviceDescriptors::new()?;
        Ok(descriptors.iter().count())
    }

    /// List all available RTL-SDR devices
    pub fn list_devices() -> Result<Vec<DeviceDescriptor>> {
        let descriptors = DeviceDescriptors::new()?;
        Ok(descriptors.iter().collect())
    }

    /// Open the first available RTL-SDR device
    pub fn open_first_available() -> Result<RtlSdr> {
        let descriptors = DeviceDescriptors::new()?;
        let first_device = descriptors
            .iter()
            .next()
            .ok_or_else(|| RtlsdrError::RtlsdrErr("No RTL-SDR devices found".to_string()))?;
        Self::open_with_index(first_device.index)
    }

    /// Get device information for a specific device by index
    pub fn get_device_info(index: usize) -> Result<DeviceDescriptor> {
        let descriptors = DeviceDescriptors::new()?;
        let devices: Vec<DeviceDescriptor> = descriptors.iter().collect();
        devices
            .into_iter()
            .find(|d| d.index == index)
            .ok_or_else(|| RtlsdrError::RtlsdrErr(format!("No device found at index {}", index)))
    }

    /// Get the serial number for a specific device by index
    pub fn get_device_serial(index: usize) -> Result<String> {
        Self::get_device_info(index).map(|info| info.serial)
    }
}
