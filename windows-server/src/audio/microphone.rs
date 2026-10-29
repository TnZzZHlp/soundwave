use std::{
    ptr, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        AUDCLNT_STREAMOPTIONS_RAW, AudioCategory_Other, AudioClientProperties, IAudioCaptureClient,
        IAudioClient, IAudioClient2, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
        eCapture, eConsole,
    },
    System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    },
};
use windows::core::Interface;

use crate::loudness::{LevelWindow, median_level};

struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub struct Measurement {
    pub level_dbfs: f64,
    pub spread_db: f64,
    pub tone_windows: usize,
    pub windows: usize,
    pub raw: bool,
}

impl Measurement {
    pub fn tone_detected(&self) -> bool {
        self.windows != 0 && self.tone_windows * 4 >= self.windows * 3
    }

    pub fn diagnostics(&self, background: f64) -> String {
        format!(
            "input {:.1} dBFS, ambient {:.1} dBFS, test windows {}/{}, {} input",
            self.level_dbfs,
            background,
            self.tone_windows,
            self.windows,
            if self.raw { "RAW" } else { "processed" }
        )
    }
}

pub struct Microphone {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pub raw: bool,
    _com: Com,
}

impl Microphone {
    pub fn open() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("Could not initialize microphone capture")?;
        let com = Com;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) }.context(
            "No default Windows microphone; check input device and microphone privacy settings",
        )?;
        let mut client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
        let properties = AudioClientProperties {
            cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
            eCategory: AudioCategory_Other,
            Options: AUDCLNT_STREAMOPTIONS_RAW,
            ..Default::default()
        };
        let mut raw = client
            .cast::<IAudioClient2>()
            .is_ok_and(|client| unsafe { client.SetClientProperties(&properties).is_ok() });
        let format = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 1,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 192_000,
            nBlockAlign: 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let initialize = |client: &IAudioClient| unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000,
                0,
                &format,
                None,
            )
        };
        let mut initialized = initialize(&client);
        if raw && initialized.is_err() {
            client = unsafe { device.Activate(CLSCTX_ALL, None) }?;
            raw = false;
            initialized = initialize(&client);
        }
        initialized.context(
            "Could not open microphone; check Windows microphone access for desktop apps",
        )?;
        let capture = unsafe { client.GetService::<IAudioCaptureClient>() }?;
        unsafe { client.Start() }?;
        Ok(Self {
            client,
            capture,
            raw,
            _com: com,
        })
    }

    pub fn measure(&mut self, cancel: &CancellationToken) -> Result<Measurement> {
        unsafe {
            self.client.Stop()?;
            self.client.Reset()?;
            self.client.Start()?;
        }
        let start = Instant::now();
        let settle = Duration::from_millis(800);
        let duration = Duration::from_millis(2000);
        let mut window = LevelWindow::default();
        let mut count = 0;
        let mut levels = Vec::with_capacity(12);
        let mut tone_windows = 0;
        while start.elapsed() < duration {
            anyhow::ensure!(!cancel.is_cancelled(), "Calibration cancelled");
            while unsafe { self.capture.GetNextPacketSize() }? > 0 {
                anyhow::ensure!(!cancel.is_cancelled(), "Calibration cancelled");
                if start.elapsed() >= duration {
                    break;
                }
                let mut data = ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                unsafe {
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                }?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let invalid = frames > 48_000 || (!silent && data.is_null() && frames != 0);
                let samples = if !invalid && !silent && frames > 0 {
                    unsafe { std::slice::from_raw_parts(data, frames as usize * 4) }.to_vec()
                } else {
                    Vec::new()
                };
                unsafe { self.capture.ReleaseBuffer(frames) }?;
                anyhow::ensure!(!invalid, "Invalid microphone capture packet");
                if start.elapsed() < settle {
                    continue;
                }
                if flags
                    & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0
                        | AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0) as u32
                    != 0
                {
                    bail!("Microphone capture was interrupted; try again");
                }
                for index in 0..frames as usize {
                    let sample = if silent {
                        0.0
                    } else {
                        f32::from_le_bytes(samples[index * 4..index * 4 + 4].try_into()?)
                    };
                    window.push(f64::from(sample));
                    count += 1;
                    if count == 4800 {
                        levels.push(window.finish().map_err(anyhow::Error::msg)?);
                        tone_windows += usize::from(window.tone_present());
                        window = LevelWindow::default();
                        count = 0;
                    }
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        let (level, spread) = median_level(&mut levels).map_err(anyhow::Error::msg)?;
        Ok(Measurement {
            level_dbfs: level,
            spread_db: spread,
            tone_windows,
            windows: levels.len(),
            raw: self.raw,
        })
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}
