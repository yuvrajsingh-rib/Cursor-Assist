//! Microphone capture for dictation: records the default input device and hands back a
//! 16 kHz mono 16-bit WAV, the format speech-to-text servers expect.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

const TARGET_RATE: u32 = 16_000;
/// Dictation is for short instructions; stop on our own if the user forgets.
const MAX_DURATION: Duration = Duration::from_secs(90);
const MIN_DURATION: Duration = Duration::from_millis(400);
const LEVEL_INTERVAL: Duration = Duration::from_millis(60);

enum Control {
    Stop,
    Cancel,
}

/// A recording in progress. Dropping it without `stop` discards the audio.
pub struct Recording {
    control: mpsc::Sender<Control>,
    result: mpsc::Receiver<Result<Vec<u8>, String>>,
}

impl Recording {
    /// Starts recording on a dedicated thread (audio streams can't move between threads).
    /// `on_level` receives a 0..1 loudness value a few times per second, for a level meter.
    pub fn start(on_level: impl Fn(f32) + Send + 'static) -> Result<Self, String> {
        let (control_tx, control_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
                let level = Arc::new(AtomicU32::new(0));
                let (stream, rate) = match open_stream(samples.clone(), level.clone()) {
                    Ok(opened) => opened,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));

                let started = Instant::now();
                let outcome = loop {
                    match control_rx.recv_timeout(LEVEL_INTERVAL) {
                        Ok(Control::Stop) => break Some(()),
                        Ok(Control::Cancel) | Err(RecvTimeoutError::Disconnected) => break None,
                        Err(RecvTimeoutError::Timeout) if started.elapsed() > MAX_DURATION => break Some(()),
                        Err(RecvTimeoutError::Timeout) => on_level(f32::from_bits(level.load(Ordering::Relaxed))),
                    }
                };
                drop(stream);
                if outcome.is_none() {
                    return;
                }
                let audio = std::mem::take(&mut *samples.lock().unwrap());
                let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len().max(1) as f32).sqrt();
                eprintln!(
                    "[audio] {:.1}s at {rate} Hz, peak {peak:.3}, rms {rms:.4}",
                    audio.len() as f32 / rate as f32
                );
                let result = if audio.len() < (rate as f32 * MIN_DURATION.as_secs_f32()) as usize {
                    Err("Too short. Hold the mic a little longer.".to_string())
                } else {
                    Ok(encode_wav(&resample(&audio, rate, TARGET_RATE)))
                };
                let _ = result_tx.send(result);
            })
            .map_err(|e| e.to_string())?;

        ready_rx.recv().map_err(|_| "recorder stopped unexpectedly".to_string())??;
        Ok(Self { control: control_tx, result: result_rx })
    }

    /// Stops recording and returns the WAV bytes.
    pub fn stop(self) -> Result<Vec<u8>, String> {
        let _ = self.control.send(Control::Stop);
        self.result.recv().map_err(|_| "recording failed".to_string())?
    }

    pub fn cancel(self) {
        let _ = self.control.send(Control::Cancel);
    }
}

/// Opens the default microphone, appending mono f32 samples to `samples`. Returns the stream
/// (recording while it lives) and its sample rate.
fn open_stream(samples: Arc<Mutex<Vec<f32>>>, level: Arc<AtomicU32>) -> Result<(cpal::Stream, u32), String> {
    let device = cpal::default_host().default_input_device().ok_or("No microphone found")?;
    let supported = device.default_input_config().map_err(|e| format!("Microphone unavailable: {e}"))?;
    let rate = supported.sample_rate();
    let format = supported.sample_format();
    let config = supported.config();
    let name = device.description().map(|d| d.to_string()).unwrap_or_else(|_| "unknown device".into());
    eprintln!("[audio] recording from {name}: {format:?}, {} ch, {rate} Hz", config.channels);
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, samples, level),
        SampleFormat::I16 => build::<i16>(&device, config, samples, level),
        SampleFormat::U16 => build::<u16>(&device, config, samples, level),
        SampleFormat::I32 => build::<i32>(&device, config, samples, level),
        other => return Err(format!("Unsupported microphone format {other:?}")),
    }?;
    stream.play().map_err(|e| format!("Could not start the microphone: {e}"))?;
    Ok((stream, rate))
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    samples: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                let mut out = samples.lock().unwrap();
                let mut energy = 0.0f32;
                for frame in data.chunks(channels) {
                    let mono = frame.iter().map(|&s| f32::from_sample_(s)).sum::<f32>() / channels as f32;
                    energy += mono * mono;
                    out.push(mono);
                }
                let rms = (energy / (data.len() / channels).max(1) as f32).sqrt();
                // Speech RMS is usually well below 0.3; stretch it so the meter moves visibly.
                level.store((rms * 4.0).min(1.0).to_bits(), Ordering::Relaxed);
            },
            |e| eprintln!("[audio] stream error: {e}"),
            None,
        )
        .map_err(|e| format!("Could not open the microphone: {e}"))
}

/// Linear-interpolation resampling; plenty for speech recognition.
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let len = (input.len() as f64 / ratio) as usize;
    (0..len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx];
            let b = *input.get(idx + 1).unwrap_or(&a);
            a + (b - a) * frac
        })
        .collect()
}

fn encode_wav(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&TARGET_RATE.to_le_bytes());
    wav.extend_from_slice(&(TARGET_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        wav.extend_from_slice(&((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_halves_length_when_halving_rate() {
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let out = resample(&input, 32_000, 16_000);
        assert_eq!(out.len(), 500);
        assert_eq!(out[10], 20.0);
    }

    #[test]
    fn wav_header_describes_16khz_mono_pcm16() {
        let wav = encode_wav(&[0.0, 0.5, -0.5]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 44 + 6);
    }
}
