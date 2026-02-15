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
                
                // Panic Yakalama Mekanizması (Sessiz çöküşleri önler)
                let result = panic::catch_unwind(move || {
                    if let Err(e) = run_audio_loop(is_running.clone(), socket, target, rx_cnt, tx_cnt) {
                        error!("🔥 CRITICAL AUDIO ENGINE FAILURE: {:?}", e);
                        // Burada normalde EventBus üzerinden UI'a 'MicError' gönderilmeli.
                        // Şimdilik is_running'i false yaparak durumu bildiriyoruz.
                        is_running.store(false, Ordering::SeqCst);
                    }
                });

                if let Err(err) = result {
                    error!("☠️ RTP THREAD PANIC: {:?}", err);
                }
                
                info!("🛑 RTP Worker Thread Exited.");
            })
            .expect("Failed to spawn RTP thread");
    }

    pub fn stop(&self) {
        info!("🛑 Stopping RTP Engine...");
        self.is_running.store(false, Ordering::SeqCst);
        // İstatistikleri sıfırlama, son rapor için kalsın.
    }
}

fn run_audio_loop(
    is_running: Arc<AtomicBool>, 
    socket: Arc<UdpSocket>, 
    target: SocketAddr,
    rx_cnt: Arc<AtomicU64>,
    tx_cnt: Arc<AtomicU64>
) -> anyhow::Result<()> {
    // 1. Host Seçimi (Platforma göre en iyisini seçmeye çalışır)
    let host = cpal::default_host();
    info!("🎤 Audio Host: {:?}", host.id());

    // 2. Cihaz Seçimi
    let input_device = host.default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No Default Input Device Found"))?;
    
    let output_device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No Default Output Device Found"))?;

    info!("🎤 Input Device: {}", input_device.name().unwrap_or("Unknown".into()));
    info!("🔊 Output Device: {}", output_device.name().unwrap_or("Unknown".into()));

    // 3. Konfigürasyon (Supported Configs içinden en uygununu bul)
    // Standart config yerine, cihazın desteklediği ilk geçerli konfigürasyonu alıyoruz.
    let supported_input_configs = input_device.supported_input_configs()?;
    let input_config_range = supported_input_configs
        .filter(|c| c.channels() == 1 || c.channels() == 2) // Mono veya Stereo
        .max_by_key(|c| c.max_sample_rate()) // En yüksek kaliteyi dene
        .ok_or_else(|| anyhow::anyhow!("No supported input config found"))?;

    let input_config: cpal::StreamConfig = input_config_range.with_max_sample_rate().into();
    let output_config: cpal::StreamConfig = output_device.default_output_config()?.into();

    let hw_sample_rate = input_config.sample_rate.0 as usize;
    info!("🎛️ HW Sample Rate: {} Hz", hw_sample_rate);

    // 4. Ring Buffer Kurulumu
    let rb_in = HeapRb::<f32>::new(8192); // Buffer boyutu artırıldı
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    let rb_out = HeapRb::<f32>::new(8192);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    // 5. Stream Oluşturma (Hata yakalama ile)
    let err_fn = |err| error!("Audio Stream Error: {}", err);

    let input_stream = input_device.build_input_stream(
        &input_config, 
        move |data: &[f32], _: &_| {
            // Basit Gain Kontrolü (Mikrofon sesi çok düşükse artırmak için)
            // Şimdilik ham veriyi alıyoruz.
            for &s in data { 
                let _ = mic_prod.push(s); 
            }
        }, 
        err_fn, 
        None
    )?;

    let output_stream = output_device.build_output_stream(
        &output_config, 
        move |data: &mut [f32], _: &_| {
            for s in data.iter_mut() { 
                *s = spk_cons.pop().unwrap_or(0.0); 
            }
        }, 
        err_fn, 
        None
    )?;

    // 6. Başlatma
    input_stream.play()?;
    output_stream.play()?;
    info!("▶️ Audio Hardware Streams Started Successfully");

    // 7. Kodek ve DSP Hazırlığı
    let profile = AudioProfile::default();
    let codec_type = profile.preferred_audio_codec();
    let payload_type = profile.get_by_payload(codec_type as u8).map(|c| c.payload_type).unwrap_or(0);

    let mut encoder = CodecFactory::create_encoder(codec_type);
    let mut decoder = CodecFactory::create_decoder(codec_type);
    
    // G.729 20ms = 160 samples @ 8kHz
    let ptime_ms = profile.ptime as u64;
    let mut pacer = Pacer::new(ptime_ms);
    
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    let sample_per_frame = codec_type.samples_per_frame(profile.ptime);
    
    let mut recv_buf = [0u8; 1500];

    info!("🎙️ RTP Loop Active. Streaming to {} using {:?}", target, codec_type);

    // 8. Ana Döngü
    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        // --- TX: Mikrofondan Oku ve Gönder ---
        let mut mic_data = Vec::with_capacity(sample_per_frame * 2); // Kabaca
        while let Some(s) = mic_cons.pop() { 
            // Float (-1.0..1.0) -> i16 (-32768..32767)
            let s_clamped = s.clamp(-1.0, 1.0);
            mic_data.push((s_clamped * 32767.0) as i16); 
        }

        if !mic_data.is_empty() {
            // Resample HW Rate -> 8kHz
            let resampled = simple_resample(&mic_data, hw_sample_rate, 8000);
            
            // Frameleme (Codec'in istediği boyutta parçala)
            for chunk in resampled.chunks(sample_per_frame) {
                // Sadece tam frameleri gönder (Padding yapma, G.729 sevmez)
                if chunk.len() < sample_per_frame { continue; }
                
                let payload = encoder.encode(chunk);
                if payload.is_empty() { continue; } // VAD veya Encoder hatası

                let header = RtpHeader::new(payload_type, seq, ts, ssrc);
                let packet = RtpPacket { header, payload };
                
                match socket.try_send_to(&packet.to_bytes(), target) {
                    Ok(_) => {
                        tx_cnt.fetch_add(1, Ordering::Relaxed);
                    },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Soket dolu, paketi atla (Real-time sistemde bekleme yapılmaz)
                    },
                    Err(e) => error!("RTP Send Error: {}", e),
                }
                
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(sample_per_frame as u32);
            }
        }

        // --- RX: Ağdan Oku ve Hoparlöre Ver ---
        loop {
             match socket.try_recv_from(&mut recv_buf) {
                 Ok((len, src)) => {
                     // Sadece hedef sunucudan gelen paketleri kabul et (Security)
                     if src.ip() == target.ip() && len > 12 {
                         rx_cnt.fetch_add(1, Ordering::Relaxed);
                         
                         let payload = &recv_buf[12..len];
                         // Payload Type kontrolü burada yapılabilir
                         
                         let samples_8k = decoder.decode(payload);
                         // Resample 8kHz -> HW Rate
                         let resampled_out = simple_resample(&samples_8k, 8000, hw_sample_rate);
                         
                         for s in resampled_out { 
                             let _ = spk_prod.push(s as f32 / 32768.0); 
                         }
                     }
                 },
                 Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // Veri yok
                 Err(e) => {
                     error!("RTP Recv Error: {}", e);
                     break; 
                 },
             }
        }
    }
    
    info!("🛑 Audio Loop Finished Cleanly.");
    Ok(())
}