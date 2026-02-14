// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use tracing::{info, error};
use sentiric_rtp_core::{AudioProfile, CodecFactory, Pacer, RtpHeader, RtpPacket, simple_resample};

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
        if self.is_running.swap(true, Ordering::SeqCst) { return; }
        
        let is_running = self.is_running.clone();
        let socket = self.socket.clone();
        let rx_cnt = self.rx_count.clone();
        let tx_cnt = self.tx_count.clone();

        std::thread::spawn(move || {
            if let Err(e) = run_audio_loop(is_running, socket, target, rx_cnt, tx_cnt) {
                error!("RTP Audio Loop Failure: {}", e);
            }
        });
    }

    pub fn stop(&self) {
        self.is_running.store(false, Ordering::SeqCst);
        self.rx_count.store(0, Ordering::SeqCst);
        self.tx_count.store(0, Ordering::SeqCst);
    }
}

fn run_audio_loop(
    is_running: Arc<AtomicBool>, 
    socket: Arc<UdpSocket>, 
    target: SocketAddr,
    rx_cnt: Arc<AtomicU64>,
    tx_cnt: Arc<AtomicU64>
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let input_device = host.default_input_device().ok_or_else(|| anyhow::anyhow!("No Mic"))?;
    let output_device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("No Spk"))?;
    let config: cpal::StreamConfig = input_device.default_input_config()?.into();
    let hw_sample_rate = config.sample_rate.0 as usize;

    let rb_in = HeapRb::<f32>::new(4096);
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    let rb_out = HeapRb::<f32>::new(4096);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    let input_stream = input_device.build_input_stream(&config, move |data: &[f32], _| {
        for &s in data { let _ = mic_prod.push(s); }
    }, |e| error!("Mic Stream Error: {}", e), None)?;

    let output_stream = output_device.build_output_stream(&config, move |data: &mut [f32], _| {
        for s in data.iter_mut() { *s = spk_cons.pop().unwrap_or(0.0); }
    }, |e| error!("Spk Stream Error: {}", e), None)?;

    input_stream.play()?;
    output_stream.play()?;

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

    info!("🎙️ RTP Media Flowing to {}", target);

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        let mut mic_data = Vec::new();
        while let Some(s) = mic_cons.pop() { mic_data.push((s * 32767.0) as i16); }

        if !mic_data.is_empty() {
            let resampled = simple_resample(&mic_data, hw_sample_rate, 8000);
            for chunk in resampled.chunks(sample_per_frame) {
                if chunk.len() < sample_per_frame { continue; }
                let payload = encoder.encode(chunk);
                let header = RtpHeader::new(payload_type, seq, ts, ssrc);
                let packet = RtpPacket { header, payload };
                if socket.try_send_to(&packet.to_bytes(), target).is_ok() {
                    tx_cnt.fetch_add(1, Ordering::Relaxed);
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(sample_per_frame as u32);
            }
        }

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
    Ok(())
}