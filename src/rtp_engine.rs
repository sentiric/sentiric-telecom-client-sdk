// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use tracing::{info, error, warn};
use sentiric_rtp_core::{AudioProfile, CodecFactory, Pacer, RtpHeader, RtpPacket, simple_resample};
use std::panic;

pub struct RtpEngine {
    socket: Arc<UdpSocket>,
    is_running: Arc<AtomicBool>,
    pub rx_count: Arc<AtomicU64>,
    pub tx_count: Arc<AtomicU64>,
}

impl RtpEngine {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        Self {
            socket,
            is_running: Arc::new(AtomicBool::new(false)),
            rx_count: Arc::new(AtomicU64::new(0)),
            tx_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn start(&self, target: SocketAddr) {
        if self.is_running.swap(true, Ordering::SeqCst) { 
            warn!("⚠️ RTP Engine already running. Ignoring start command.");
            return; 
        }
        
        let is_running = self.is_running.clone();
        let socket = self.socket.clone();
        let rx_cnt = self.rx_count.clone();
        let tx_cnt = self.tx_count.clone();

        // [HARDENING]: Thread isimlendirme ve Panic Yakalama
        std::thread::Builder::new()
            .name("sentiric-rtp-worker".to_string())
            .spawn(move || {
                info!("🎧 RTP Worker Thread Started. Target: {}", target);
                
                // [FIX]: Closure içine taşınacak kopya oluşturuluyor.
                // Orijinal 'is_running' değişkeni bu scope'ta (panic handler için) kalacak.
                let is_running_inner = is_running.clone();

                // Panic Yakalama Mekanizması (Sessiz çöküşleri önler)
                let result = panic::catch_unwind(move || {
                    // İçeriye sadece inner kopyayı taşıyoruz
                    if let Err(e) = run_audio_loop(is_running_inner.clone(), socket, target, rx_cnt, tx_cnt) {
                        error!("🔥 CRITICAL AUDIO ENGINE FAILURE: {:?}", e);
                        // Mantıksal hata durumunda inner kopya ile flag indirilir
                        is_running_inner.store(false, Ordering::SeqCst);
                    }
                });

                // Panic durumunda (Thread çökerse) dışarıdaki kopya ile flag indirilir
                if let Err(err) = result {
                    error!("☠️ RTP THREAD PANIC: {:?}", err);
                    is_running.store(false, Ordering::SeqCst);
                }
                
                info!("🛑 RTP Worker Thread Exited.");
            })
            .expect("Failed to spawn RTP thread");
    }

    pub fn stop(&self) {
        info!("🛑 Stopping RTP Engine...");
        self.is_running.store(false, Ordering::SeqCst);
    }
}

fn run_audio_loop(
    is_running: Arc<AtomicBool>, 
    socket: Arc<UdpSocket>, 
    target: SocketAddr,
    rx_cnt: Arc<AtomicU64>,
    tx_cnt: Arc<AtomicU64>
) -> anyhow::Result<()> {
    // 1. Host Seçimi
    let host = cpal::default_host();
    info!("🎤 Audio Host ID: {:?}", host.id());

    // 2. Cihaz Seçimi (Robust Discovery)
    let input_device = host.default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No Default Input Device Found"))?;
    
    let output_device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No Default Output Device Found"))?;

    info!("🎤 Input: {}", input_device.name().unwrap_or("Unknown".into()));
    info!("🔊 Output: {}", output_device.name().unwrap_or("Unknown".into()));

    // 3. Konfigürasyon (Otomatik Uyum)
    let config: cpal::StreamConfig = input_device.default_input_config()
        .map_err(|e| anyhow::anyhow!("Input Config Error: {}", e))?
        .into();
        
    let hw_sample_rate = config.sample_rate.0 as usize;
    info!("🎛️ HW Sample Rate: {} Hz", hw_sample_rate);

    // 4. Ring Buffer (Latency Tuning: 4096 -> 8192)
    let rb_in = HeapRb::<f32>::new(8192);
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    let rb_out = HeapRb::<f32>::new(8192);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    // 5. Stream Error Callback
    let err_fn = |err| error!("Audio Stream Callback Error: {}", err);

    let input_stream = input_device.build_input_stream(
        &config, 
        move |data: &[f32], _: &_| {
            for &s in data { let _ = mic_prod.push(s); }
        }, 
        err_fn, 
        None
    ).map_err(|e| anyhow::anyhow!("Failed to build input stream: {}", e))?;

    let output_config: cpal::StreamConfig = output_device.default_output_config()?.into();
    let output_stream = output_device.build_output_stream(
        &output_config, 
        move |data: &mut [f32], _: &_| {
            for s in data.iter_mut() { *s = spk_cons.pop().unwrap_or(0.0); }
        }, 
        err_fn, 
        None
    ).map_err(|e| anyhow::anyhow!("Failed to build output stream: {}", e))?;

    // 6. Başlatma
    input_stream.play().map_err(|e| anyhow::anyhow!("Failed to start input: {}", e))?;
    output_stream.play().map_err(|e| anyhow::anyhow!("Failed to start output: {}", e))?;
    
    info!("▶️ Audio Hardware Streams Running...");

    // 7. DSP & Codec Loop
    let profile = AudioProfile::default();
    let codec_type = profile.preferred_audio_codec();
    let payload_type = profile.get_by_payload(codec_type as u8).map(|c| c.payload_type).unwrap_or(0);

    let mut encoder = CodecFactory::create_encoder(codec_type);
    let mut decoder = CodecFactory::create_decoder(codec_type);
    let mut pacer = Pacer::new(profile.ptime as u64);
    
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    let sample_per_frame = codec_type.samples_per_frame(profile.ptime);
    let mut recv_buf = [0u8; 1500];

    info!("🎙️ RTP Loop Active -> {}", target);

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        // --- TX ---
        let mut mic_data = Vec::with_capacity(sample_per_frame * 2);
        while let Some(s) = mic_cons.pop() { 
            let s_clamped = s.clamp(-1.0, 1.0);
            mic_data.push((s_clamped * 32767.0) as i16); 
        }

        if !mic_data.is_empty() {
            let resampled = simple_resample(&mic_data, hw_sample_rate, 8000);
            for chunk in resampled.chunks(sample_per_frame) {
                if chunk.len() < sample_per_frame { continue; }
                let payload = encoder.encode(chunk);
                if payload.is_empty() { continue; }

                let header = RtpHeader::new(payload_type, seq, ts, ssrc);
                let packet = RtpPacket { header, payload };
                
                match socket.try_send_to(&packet.to_bytes(), target) {
                    Ok(_) => {
                        tx_cnt.fetch_add(1, Ordering::Relaxed);
                    },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Soket dolu, atla
                    },
                    Err(e) => error!("RTP Send Error: {}", e),
                }
                
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(sample_per_frame as u32);
            }
        }

        // --- RX ---
        loop {
             match socket.try_recv_from(&mut recv_buf) {
                 Ok((len, src)) => {
                     if src.ip() == target.ip() && len > 12 {
                         rx_cnt.fetch_add(1, Ordering::Relaxed);
                         let payload = &recv_buf[12..len];
                         let samples_8k = decoder.decode(payload);
                         let resampled_out = simple_resample(&samples_8k, 8000, hw_sample_rate);
                         for s in resampled_out { let _ = spk_prod.push(s as f32 / 32768.0); }
                     }
                 },
                 Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                 Err(_) => break,
             }
        }
    }
    
    info!("🛑 Audio Loop Stopped.");
    Ok(())
}