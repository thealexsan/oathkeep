use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, Sleep, WaitForSingleObject,
};
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

const RATE: u32 = 48_000;
const FRAME_BYTES: usize = 4;

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated;

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

const TARGET: &str = "OATHKEEP_AUDIO_CAPTURE_OF";

pub fn start() {
    if tracing::enabled!(tracing::Level::DEBUG) {
        let _ = std::thread::Builder::new()
            .name("oathkeep timing probe".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let at = Instant::now();
                    // SAFETY: plain wait.
                    unsafe { Sleep(1) };
                    let ms = at.elapsed().as_secs_f32() * 1000.0;
                    tracing::debug!(ms, "Sleep(1) took");
                }
            });
    }
    if std::env::var_os("OATHKEEP_AUDIO_CAPTURE").is_some()
        && let Err(e) = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .env(TARGET, std::process::id().to_string())
                .spawn()
        })
    {
        tracing::warn!("audio capture not started: {e}");
    }
}

pub fn child() -> bool {
    let (Some(pid), Some(path)) = (
        std::env::var(TARGET).ok().and_then(|p| p.parse().ok()),
        std::env::var_os("OATHKEEP_AUDIO_CAPTURE"),
    ) else {
        return false;
    };
    if let Err(e) = record(pid, Path::new(&path)) {
        tracing::warn!("audio capture stopped: {e}");
    }
    true
}

fn header(frames: u64) -> [u8; 44] {
    let data = u32::try_from(frames * FRAME_BYTES as u64).unwrap_or(u32::MAX - 36);
    let mut h = [0u8; 44];
    let fields: [(&[u8], usize); 11] = [
        (b"RIFF", 0),
        (&(36 + data).to_le_bytes(), 4),
        (b"WAVEfmt ", 8),
        (&16u32.to_le_bytes(), 16),
        (&1u16.to_le_bytes(), 20),
        (&2u16.to_le_bytes(), 22),
        (&RATE.to_le_bytes(), 24),
        (&(RATE * FRAME_BYTES as u32).to_le_bytes(), 28),
        (&(FRAME_BYTES as u16).to_le_bytes(), 32),
        (&16u16.to_le_bytes(), 34),
        (b"data", 36),
    ];
    for (bytes, at) in fields {
        h[at..at + bytes.len()].copy_from_slice(bytes);
    }
    h[40..].copy_from_slice(&data.to_le_bytes());
    h
}

fn record(pid: u32, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: COM calls with parameters built here; buffers are used only
    // between `GetBuffer` and `ReleaseBuffer`.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let target = OpenProcess(PROCESS_SYNCHRONIZE, false, pid)?;
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        #[repr(C)]
        struct Blob {
            vt: u16,
            reserved: [u16; 3],
            size: u32,
            data: *const AUDIOCLIENT_ACTIVATION_PARAMS,
        }
        let blob = Blob {
            vt: 0x41,
            reserved: [0; 3],
            size: size_of_val(&params) as u32,
            data: &params,
        };
        let handler: IActivateAudioInterfaceCompletionHandler = Activated.into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((&raw const blob).cast()),
            &handler,
        )?;
        let client = loop {
            let (mut hr, mut client) = (HRESULT(0), None::<IUnknown>);
            if operation.GetActivateResult(&mut hr, &mut client).is_ok() {
                hr.ok()?;
                break client.ok_or("no audio client")?.cast::<IAudioClient>()?;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * FRAME_BYTES as u32,
            nBlockAlign: FRAME_BYTES as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            10_000_000,
            0,
            &format,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        let mut file = File::create(path)?;
        file.write_all(&header(0))?;
        client.Start()?;
        tracing::info!(path = %path.display(), "recording the title's audio");

        let (mut written, mut first) = (0u64, None);
        let mut patched = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let ended = WaitForSingleObject(target, 0) == WAIT_OBJECT_0;
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0, 0, 0);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let at = (qpc - *first.get_or_insert(qpc)) * u64::from(RATE) / 10_000_000;
                if at > written + u64::from(RATE / 500) {
                    tracing::debug!(
                        ms = (at - written) * 1000 / u64::from(RATE),
                        "audio capture: nothing played"
                    );
                    let silence = vec![0u8; (at - written) as usize * FRAME_BYTES];
                    file.write_all(&silence)?;
                    written = at;
                }
                let bytes = frames as usize * FRAME_BYTES;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    file.write_all(&vec![0u8; bytes])?;
                } else {
                    file.write_all(std::slice::from_raw_parts(data, bytes))?;
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && written > 0 {
                    tracing::debug!("audio capture: discontinuity");
                }
                written += u64::from(frames);
                capture.ReleaseBuffer(frames)?;
            }
            if ended || patched.elapsed() >= Duration::from_secs(1) {
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&header(written))?;
                file.seek(SeekFrom::End(0))?;
                patched = Instant::now();
            }
            if ended {
                return Ok(());
            }
        }
    }
}
