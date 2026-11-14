//! Sound for the player: the clip's audio decoded by ffmpeg to float PCM at
//! the output device's own rate and channel count, played through cpal.
//!
//! While audio plays it is the playback clock: the video shows whatever
//! frame matches the samples actually heard, so the two cannot drift apart
//! however long the clip. If decoding falls behind, the device gets silence
//! and the clock stops with it.
//!
//! Each stream's volume is its level in the mix; the listening volume is one
//! for them all (`set_listening`), so the meter (`levels`) can sum what the
//! streams send as the export mixes them, whatever the speakers are set to.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How much decoded audio to keep ahead of the device.
const BUFFER_SECONDS: f64 = 0.5;

/// The speakers' volume (f32 bits), on top of every stream's own.
static LISTENING: AtomicU32 = AtomicU32::new(0x3f80_0000);

pub fn set_listening(volume: f32) {
    LISTENING.store(volume.to_bits(), Ordering::Relaxed);
}

/// What every stream sends at its mix level, summed per output frame (left
/// and right) by when it reaches the speakers, for the meter. The streams
/// share one device, so one rate; a callback's frames are placed by the
/// time it ran plus the device's latency, which aligns them within a
/// callback's jitter, plenty for a meter.
struct Meter {
    rate: f64,
    /// A ring of (output frame, summed left and right); a slot holding
    /// another frame is silence.
    slots: Vec<(u64, [f32; 2])>,
}

/// Seconds the meter's ring covers.
const METER_SECONDS: f64 = 0.5;

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static METER: Mutex<Meter> = Mutex::new(Meter { rate: 0.0, slots: Vec::new() });

/// Peak and RMS per channel (left, right), linear, of the mix reaching the
/// speakers over the last `window` seconds; silence when nothing plays.
pub fn levels(window: f64) -> ([f32; 2], [f32; 2]) {
    let m = METER.lock().unwrap_or_else(|e| e.into_inner());
    let (mut peak, mut sum) = ([0f32; 2], [0f64; 2]);
    if m.rate <= 0.0 || m.slots.is_empty() {
        return (peak, [0.0; 2]);
    }
    let now = (EPOCH.elapsed().as_secs_f64() * m.rate) as u64;
    let n = ((window * m.rate) as u64).clamp(1, m.slots.len() as u64);
    for f in now.saturating_sub(n)..now {
        let (at, s) = m.slots[(f % m.slots.len() as u64) as usize];
        if at == f {
            for c in 0..2 {
                peak[c] = peak[c].max(s[c].abs());
                sum[c] += (s[c] as f64).powi(2);
            }
        }
    }
    (peak, sum.map(|s| (s / n as f64).sqrt() as f32))
}

struct Shared {
    samples: Mutex<VecDeque<f32>>,
    /// Frames handed to the device so far.
    played: AtomicU64,
    /// The device's latency from callback to speaker, in microseconds.
    latency_us: AtomicU64,
    /// f32 bits.
    volume: AtomicU32,
    /// Until `play`, the device gets silence and the buffer is left alone.
    /// Pausing the stream instead isn't enough: not every backend can (on
    /// PipeWire's ALSA plugin a prepared cut's sound played under the cut
    /// before it, its buffer drained, so its clock ran early).
    running: AtomicBool,
    stop: AtomicBool,
    /// The sound has all been decoded. Once it has also all been played,
    /// the silence after it counts towards the clock: a clip's sound often
    /// ends a frame or so before its picture (our own exports' do, by
    /// ~20 ms), and a clock stopped there never reached the cut's out point,
    /// so playback stood still.
    done: AtomicBool,
}

pub struct Audio {
    _stream: cpal::Stream,
    child: Child,
    shared: Arc<Shared>,
    rate: f64,
}

impl Audio {
    /// Starts playing `input`'s first audio track from `start` seconds.
    pub fn start(input: &Path, start: f64, volume: f32) -> Result<Audio> {
        let audio = Audio::prepare(input, start, volume)?;
        audio.play();
        Ok(audio)
    }

    /// Opens the device and starts decoding from `start`, filling the buffer,
    /// but plays silence until `play`: the next cut's sound, ready ahead, so
    /// moving on to it needn't wait for the device or ffmpeg.
    pub fn prepare(input: &Path, start: f64, volume: f32) -> Result<Audio> {
        let device = cpal::default_host().default_output_device().context("no audio output device")?;
        let supported = device.default_output_config().context("querying the audio device")?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let channels = config.channels as usize;
        let rate = config.sample_rate as f64;

        let shared = Arc::new(Shared {
            samples: Mutex::new(VecDeque::new()),
            played: AtomicU64::new(0),
            latency_us: AtomicU64::new(0),
            volume: AtomicU32::new(volume.to_bits()),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });

        // The device first: where its stream won't open (busy, an odd
        // sample format) nothing else has been started. ffmpeg and its
        // reader came first once, and were left running at every failure,
        // which the music made again each frame.
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, shared.clone()),
            SampleFormat::I16 => build::<i16>(&device, &config, shared.clone()),
            SampleFormat::I32 => build::<i32>(&device, &config, shared.clone()),
            SampleFormat::U16 => build::<u16>(&device, &config, shared.clone()),
            other => bail!("unsupported audio sample format {other}"),
        }?;
        stream.play().context("starting audio")?;
        let mut child = crate::ffmpeg::tool("ffmpeg")
            .args(["-v", "error", "-nostdin", "-ss", &start.to_string(), "-i"])
            .arg(input)
            .args(["-map", "0:a:0", "-f", "f32le", "-ac", &channels.to_string(), "-ar", &config.sample_rate.to_string(), "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running ffmpeg")?;
        let mut out = child.stdout.take().unwrap();
        let reader = shared.clone();
        let ahead = (BUFFER_SECONDS * rate) as usize * channels;
        thread::spawn(move || {
            let mut bytes = vec![0u8; 4096 * 4];
            // However the loop ends, there's no more sound to come.
            struct Done(Arc<Shared>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.done.store(true, Ordering::Relaxed);
                }
            }
            let _done = Done(reader.clone());
            while !reader.stop.load(Ordering::Relaxed) {
                if reader.samples.lock().unwrap().len() > ahead {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let Ok(n) = out.read(&mut bytes) else { break };
                if n == 0 {
                    break;
                }
                // A read can end mid-sample; keep only whole ones.
                let whole = n / 4 * 4;
                let mut q = reader.samples.lock().unwrap();
                q.extend(bytes[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                drop(q);
                if whole < n {
                    let rest = n - whole;
                    let mut tail = vec![0u8; 4 - rest];
                    if out.read_exact(&mut tail).is_err() {
                        break;
                    }
                    let mut b = [0u8; 4];
                    b[..rest].copy_from_slice(&bytes[whole..n]);
                    b[rest..].copy_from_slice(&tail);
                    reader.samples.lock().unwrap().push_back(f32::from_le_bytes(b));
                }
            }
        });

        Ok(Audio { _stream: stream, child, shared, rate })
    }

    /// Lets the sound through: from the next device callback, milliseconds.
    pub fn play(&self) {
        self.shared.running.store(true, Ordering::Relaxed);
    }

    /// Seconds of audio heard since `start`.
    pub fn elapsed(&self) -> f64 {
        let played = self.shared.played.load(Ordering::Relaxed) as f64 / self.rate;
        (played - self.shared.latency_us.load(Ordering::Relaxed) as f64 / 1e6).max(0.0)
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.to_bits(), Ordering::Relaxed);
    }

    /// Seconds from handing samples to the device to hearing them, as the
    /// device last reported (0 before its first callback).
    pub fn latency(&self) -> f64 {
        self.shared.latency_us.load(Ordering::Relaxed) as f64 / 1e6
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn build<T: SizedSample + FromSample<f32>>(device: &cpal::Device, config: &StreamConfig, shared: Arc<Shared>) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let rate = config.sample_rate as f64;
    let mut next = 0u64;
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], info: &OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback);
            shared.latency_us.store(latency.as_micros() as u64, Ordering::Relaxed);
            if !shared.running.load(Ordering::Relaxed) {
                data.fill(T::EQUILIBRIUM);
                return;
            }
            let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            let listening = f32::from_bits(LISTENING.load(Ordering::Relaxed));
            let mut q = shared.samples.lock().unwrap();
            // Only whole frames count towards the clock; an underrun plays
            // silence without advancing it, unless the sound has ended.
            let frames = (q.len() / channels).min(data.len() / channels);
            let counted = if frames < data.len() / channels && q.len() < channels && shared.done.load(Ordering::Relaxed) { data.len() / channels } else { frames };
            let mut meter = METER.lock().unwrap_or_else(|e| e.into_inner());
            if meter.rate != rate {
                *meter = Meter { rate, slots: vec![(u64::MAX, [0.0; 2]); (METER_SECONDS * rate) as usize] };
            }
            // Never over this stream's last frames, which would count it twice.
            let first = (((EPOCH.elapsed() + latency).as_secs_f64() * rate) as u64).max(next);
            next = first + (data.len() / channels) as u64;
            let len = meter.slots.len() as u64;
            for (i, frame) in data.chunks_mut(channels).enumerate() {
                let slot = &mut meter.slots[((first + i as u64) % len) as usize];
                if slot.0 != first + i as u64 {
                    *slot = (first + i as u64, [0.0; 2]);
                }
                for (c, s) in frame.iter_mut().enumerate() {
                    let v = if i < frames { q.pop_front().unwrap_or(0.0) * volume } else { 0.0 };
                    // Mono counts on both sides; past the front pair, none.
                    if c < 2 {
                        slot.1[c] += v;
                        if channels == 1 {
                            slot.1[1] += v;
                        }
                    }
                    *s = T::from_sample(v * listening);
                }
            }
            drop(meter);
            drop(q);
            shared.played.fetch_add(counted as u64, Ordering::Relaxed);
        },
        |e| eprintln!("audio: {e}"),
        None,
    )?;
    Ok(stream)
}
