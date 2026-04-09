// Dosya: src/media/hardware.rs

use super::adapter::MediaAdapter;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info};
use ringbuf::{Consumer, HeapRb, Producer, SharedRb};
use sentiric_rtp_core::AudioResampler;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type RbProd = Producer<f32, Arc<SharedRb<f32, Vec<std::mem::MaybeUninit<f32>>>>>;
type RbCons = Consumer<f32, Arc<SharedRb<f32, Vec<std::mem::MaybeUninit<f32>>>>>;

pub struct HardwareAdapter {
    mic_cons: Mutex<RbCons>,
    spk_prod: Mutex<RbProd>,

    mic_gain: Arc<AtomicU32>,
    spk_gain: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    is_healthy: Arc<AtomicBool>,
    hw_sample_rate_in: usize,

    _keep_alive_tx: std::sync::mpsc::Sender<()>,
    mic_resampler: AudioResampler,
    spk_resampler: AudioResampler,
}

impl HardwareAdapter {
    pub fn new() -> anyhow::Result<Self> {
        let rb_in = HeapRb::<f32>::new(96000); // Bufferları 44.1k / 48k için büyüttük
        let (mut mic_prod, mic_cons) = rb_in.split();

        let rb_out = HeapRb::<f32>::new(96000);
        let (spk_prod, mut spk_cons) = rb_out.split();

        let is_muted = Arc::new(AtomicBool::new(false));
        let is_healthy = Arc::new(AtomicBool::new(true));
        let mic_gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let spk_gain = Arc::new(AtomicU32::new(1.2f32.to_bits()));

        let hw_sample_rate_in = Arc::new(AtomicU32::new(16000));
        let hw_sample_rate_out = Arc::new(AtomicU32::new(16000));

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        let (keep_alive_tx, keep_alive_rx) = std::sync::mpsc::channel::<()>();

        let t_l_mic = mic_gain.clone();
        let t_l_spk = spk_gain.clone();
        let t_l_muted = is_muted.clone();
        let t_healthy_in = is_healthy.clone();
        let t_healthy_out = is_healthy.clone();
        let t_sr_in = hw_sample_rate_in.clone();
        let t_sr_out = hw_sample_rate_out.clone();

        std::thread::Builder::new()
            .name("cpal-keeper".into())
            .spawn(move || {
                let host = cpal::default_host();

                let input_device = match host.default_input_device() {
                    Some(d) => d,
                    None => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("No input device")));
                        return;
                    }
                };

                let output_device = match host.default_output_device() {
                    Some(d) => d,
                    None => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("No output device")));
                        return;
                    }
                };

                // [CRITICAL FIX]: Android'in AudioRecord hatalarını önlemek için supported_configs()
                // taramasını atlayıp doğrudan cihazın "native" konfigürasyonunu istiyoruz.
                let input_config: cpal::StreamConfig = input_device
                    .default_input_config()
                    .map(|c| c.into())
                    .unwrap_or(cpal::StreamConfig {
                        channels: 1,
                        sample_rate: cpal::SampleRate(16000),
                        buffer_size: cpal::BufferSize::Default,
                    });

                let output_config: cpal::StreamConfig = output_device
                    .default_output_config()
                    .map(|c| c.into())
                    .unwrap_or(cpal::StreamConfig {
                        channels: 1,
                        sample_rate: cpal::SampleRate(16000),
                        buffer_size: cpal::BufferSize::Default,
                    });

                let hw_sr_in_val = input_config.sample_rate.0;
                let hw_sr_out_val = output_config.sample_rate.0;

                t_sr_in.store(hw_sr_in_val, Ordering::Relaxed);
                t_sr_out.store(hw_sr_out_val, Ordering::Relaxed);

                let in_channels = input_config.channels as usize;
                let out_channels = output_config.channels as usize;

                let mut last_sample_out = 0.0f32;

                let input_stream = match input_device.build_input_stream(
                    &input_config,
                    move |data: &[f32], _: &_| {
                        if t_l_muted.load(Ordering::Relaxed) {
                            return;
                        }
                        let gain = f32::from_bits(t_l_mic.load(Ordering::Relaxed));

                        if in_channels == 1 {
                            for &s in data {
                                let _ = mic_prod.push(s * gain);
                            }
                        } else {
                            // Cihaz stereo ise (Android 44.1k Stereo), kanalları birleştirerek Mono yapıyoruz
                            for frame in data.chunks(in_channels) {
                                let avg = frame.iter().sum::<f32>() / in_channels as f32;
                                let _ = mic_prod.push(avg * gain);
                            }
                        }
                    },
                    move |e| {
                        error!("Mic Error: {}", e);
                        t_healthy_in.store(false, Ordering::SeqCst);
                    },
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("Mic build fail: {}", e)));
                        return;
                    }
                };

                let mut is_buffering = true;

                // [CRITICAL FIX]: Sabit 3200 sample yerine, OS'in Frekansına göre DİNAMİK Jitter Buffer hesabı.
                // 250ms'lik bir Pre-Buffer ayarlıyoruz (Ağdaki Jitter'ı tamamen emer).
                // Eğer cihaz 44100 Hz ise: 44100 * 0.25 = 11025 sample bekleyecek.
                let target_prebuffer_size = (hw_sr_out_val as f32 * 0.25) as usize;

                let output_stream = match output_device.build_output_stream(
                    &output_config,
                    move |data: &mut [f32], _: &_| {
                        let gain = f32::from_bits(t_l_spk.load(Ordering::Relaxed));

                        if is_buffering {
                            if spk_cons.len() >= target_prebuffer_size {
                                is_buffering = false;
                            } else {
                                for s in data.iter_mut() {
                                    *s = 0.0;
                                }
                                return;
                            }
                        }

                        for frame in data.chunks_mut(out_channels) {
                            let sample = spk_cons.pop().unwrap_or_else(|| {
                                is_buffering = true; // Starvation! Yeniden dinamik buffer dolmasını bekle
                                last_sample_out *= 0.8; // Crackling engellemek için sinyali anında kesme, sönümle (Fade Out)
                                last_sample_out
                            });
                            last_sample_out = sample;
                            let final_sample = (sample * gain).clamp(-1.0, 1.0);
                            for s in frame.iter_mut() {
                                *s = final_sample;
                            }
                        }
                    },
                    move |e| {
                        error!("Spk Error: {}", e);
                        t_healthy_out.store(false, Ordering::SeqCst);
                    },
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("Spk build fail: {}", e)));
                        return;
                    }
                };

                if let Err(e) = input_stream.play() {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("Mic play fail: {}", e)));
                    return;
                }
                if let Err(e) = output_stream.play() {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("Spk play fail: {}", e)));
                    return;
                }

                let _ = ready_tx.send(Ok(()));
                let _ = keep_alive_rx.recv();

                info!("Hardware Adapter dropped. Audio streams closed safely.");
            })?;

        ready_rx
            .recv()
            .map_err(|e| anyhow::anyhow!("Thread communication error: {}", e))??;

        let hw_sr_in = hw_sample_rate_in.load(Ordering::Relaxed) as usize;
        let hw_sr_out = hw_sample_rate_out.load(Ordering::Relaxed) as usize;

        let mic_resampler = AudioResampler::new(hw_sr_in, 8000, 0);
        let spk_resampler = AudioResampler::new(8000, hw_sr_out, 0);

        Ok(Self {
            mic_cons: Mutex::new(mic_cons),
            spk_prod: Mutex::new(spk_prod),
            mic_gain,
            spk_gain,
            is_muted,
            is_healthy,
            hw_sample_rate_in: hw_sr_in,
            _keep_alive_tx: keep_alive_tx,
            mic_resampler,
            spk_resampler,
        })
    }
}

impl MediaAdapter for HardwareAdapter {
    fn read_mic(&self, target_8k_samples: usize) -> Vec<i16> {
        let ratio = self.hw_sample_rate_in as f32 / 8000.0;
        let hw_frame_size = (target_8k_samples as f32 * ratio).ceil() as usize;

        let mut cons = self.mic_cons.lock().unwrap();

        if self.is_muted.load(Ordering::Relaxed) {
            for _ in 0..cons.len() {
                let _ = cons.pop();
            }
            return vec![0; target_8k_samples];
        }

        let available = cons.len();
        if available < (hw_frame_size / 2) {
            return vec![];
        }

        let read_size = std::cmp::min(hw_frame_size, available);
        let mut mic_data = Vec::with_capacity(read_size);
        for _ in 0..read_size {
            let s = cons.pop().unwrap_or(0.0);
            mic_data.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
        }

        drop(cons);
        self.mic_resampler.process(&mic_data)
    }

    fn write_spk(&self, samples_8k: &[i16]) {
        let resampled = self.spk_resampler.process(samples_8k);

        let mut prod = self.spk_prod.lock().unwrap();
        for s in resampled {
            let _ = prod.push(s as f32 / 32768.0);
        }
    }

    fn is_healthy(&self) -> bool {
        self.is_healthy.load(Ordering::SeqCst)
    }

    fn set_mute(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    fn update_gains(&self, mic: f32, spk: f32) {
        self.mic_gain.store(mic.to_bits(), Ordering::Relaxed);
        self.spk_gain.store(spk.to_bits(), Ordering::Relaxed);
    }
}
