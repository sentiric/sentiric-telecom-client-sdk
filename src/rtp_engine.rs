// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use tracing::{info, error, warn};
use sentiric_rtp_core::{
    AudioProfile, CodecFactory, Pacer, RtpHeader, RtpPacket,
    simple_resample // CodecType kullanılmıyor, kaldırıldı
};


pub struct RtpEngine {
    socket: Arc<UdpSocket>,
    is_running: Arc<AtomicBool>,
}

impl RtpEngine {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        Self {
            socket,
            is_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(&self, target: SocketAddr) {
        if self.is_running.swap(true, Ordering::SeqCst) {
            return; // Zaten çalışıyor
        }
        
        let is_running = self.is_running.clone();
        let socket = self.socket.clone();

        // Audio Thread (Blocking)
        std::thread::spawn(move || {
            if let Err(e) = run_audio_loop(is_running, socket, target) {
                error!("Audio Loop Error: {}", e);
            }
        });
    }

    pub fn stop(&self) {
        self.is_running.store(false, Ordering::SeqCst);
    }
}

fn run_audio_loop(is_running: Arc<AtomicBool>, socket: Arc<UdpSocket>, target: SocketAddr) -> anyhow::Result<()> {
    let host = cpal::default_host();
    
    // Mikrofon ve Hoparlör Seçimi (Hata durumunda patlamamalı)
    let input_device = match host.default_input_device() {
        Some(d) => d,
        None => {
            warn!("Mikrofon bulunamadı! Sadece dinleme modu.");
            // Burada dummy bir device veya return gerekebilir, şimdilik error dönüyoruz.
            return Err(anyhow::anyhow!("No input device"));
        }
    };
    
    let output_device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device"))?;

    let config: cpal::StreamConfig = input_device.default_input_config()?.into();
    let hw_sample_rate = config.sample_rate.0 as usize;
    
    info!("Audio Hardware Rate: {}Hz", hw_sample_rate);

    // Bufferlar
    let rb_in = HeapRb::<f32>::new(4096);
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    
    let rb_out = HeapRb::<f32>::new(4096);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    // Input Stream
    let input_stream = input_device.build_input_stream(
        &config,
        move |data: &[f32], _| {
            for &sample in data {
                let _ = mic_prod.push(sample);
            }
        },
        |err| error!("Mic error: {}", err),
        None
    )?;

    // Output Stream
    let output_stream = output_device.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            for sample in data.iter_mut() {
                *sample = spk_cons.pop().unwrap_or(0.0);
            }
        },
        |err| error!("Spk error: {}", err),
        None
    )?;

    input_stream.play()?;
    output_stream.play()?;

    // Codec Setup (Centralized Config)
    let profile = AudioProfile::default();
    let codec_type = profile.preferred_audio_codec(); // Genellikle PCMU
    let payload_type = profile.codecs.iter()
        .find(|c| c.codec == codec_type)
        .map(|c| c.payload_type)
        .unwrap_or(0);

    let mut encoder = CodecFactory::create_encoder(codec_type);
    let mut decoder = CodecFactory::create_decoder(codec_type);

    let ptime = profile.ptime as u64; // 20ms
    let mut pacer = Pacer::new(ptime);
    
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    let sample_per_frame = codec_type.samples_per_frame(profile.ptime); // 160 sample

    let mut recv_buf = [0u8; 1500];

    info!("RTP Streaming started -> {}", target);

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        // 1. TX: Mic -> Encode -> RTP -> Network
        let mut mic_data = Vec::with_capacity(sample_per_frame * 4); // Yeterli alan
        while let Some(s) = mic_cons.pop() {
            mic_data.push((s * 32767.0) as i16);
        }

        if mic_data.len() > 0 {
            // HW Rate -> 8000Hz
            let resampled = simple_resample(&mic_data, hw_sample_rate, 8000);
            
            // Chunklar halinde gönder (Packet size kadar)
            for chunk in resampled.chunks(sample_per_frame) {
                if chunk.len() < sample_per_frame { continue; } // Tam paket değilse atla
                
                let payload = encoder.encode(chunk);
                if !payload.is_empty() {
                    let header = RtpHeader::new(payload_type, seq, ts, ssrc);
                    let packet = RtpPacket { header, payload };
                    
                    // Blocking send (UDP)
                    let _ = socket.try_send_to(&packet.to_bytes(), target);
                    
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(sample_per_frame as u32);
                }
            }
        }

        // 2. RX: Network -> RTP -> Decode -> Spk
        // Socket non-blocking olmalı veya çok kısa timeout
        // Burada basitleştirilmiş bir döngü kullanıyoruz
        loop {
             match socket.try_recv_from(&mut recv_buf) {
                 Ok((len, src)) => {
                     // Latching Check: Sadece hedeften gelenleri al
                     if src.ip() == target.ip() && len > 12 {
                         let payload = &recv_buf[12..len];
                         // Payload Type kontrolü yapılabilir
                         let samples_8k = decoder.decode(payload);
                         
                         // 8000Hz -> HW Rate
                         let resampled_out = simple_resample(&samples_8k, 8000, hw_sample_rate);
                         for s in resampled_out {
                             let _ = spk_prod.push(s as f32 / 32768.0);
                         }
                     }
                 },
                 Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // Veri yok
                 Err(_) => break,
             }
        }
    }
    
    info!("RTP Loop Stopped");
    Ok(())
}