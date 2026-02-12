// sentiric-sip-uac-core/src/lib.rs

use std::net::SocketAddr;
use tokio::net::UdpSocket as TokioUdpSocket;
use std::net::UdpSocket as StdUdpSocket; 
use tokio::sync::mpsc;
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, parser};
use sentiric_rtp_core::{RtpHeader, RtpPacket, CodecFactory, Pacer, AudioProfile, simple_resample, CodecType};
use rand::Rng;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;

/// İstemci (CLI/Mobile) tarafından dinlenecek olaylar.
#[derive(Debug, Clone)]
pub enum UacEvent {
    Log(String),
    Status(String),
    Error(String),
    CallEnded,
}

pub struct UacClient {
    event_tx: mpsc::Sender<UacEvent>,
}

impl UacClient {
    pub fn new(event_tx: mpsc::Sender<UacEvent>) -> Self {
        Self { event_tx }
    }

    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        let socket = TokioUdpSocket::bind("0.0.0.0:0").await?;
        let bound_port = socket.local_addr()?.port();
        let target_addr: SocketAddr = format!("{}:{}", target_ip, target_port).parse()?;

        // --- SIP INVITE ---
        let call_id = format!("uac-hw-{}", rand::thread_rng().gen::<u32>());
        let from = format!("<sip:{}@sentiric.mobile>;tag=uac-hw-tag", from_user);
        let to = format!("<sip:{}@{}>", to_user, target_ip);
        
        let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}@{}:{}", to_user, target_ip, target_port));
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-{}", bound_port, rand::thread_rng().gen::<u16>())));
        invite.headers.push(Header::new(HeaderName::From, from.clone()));
        invite.headers.push(Header::new(HeaderName::To, to.clone()));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@0.0.0.0:{}>", from_user, bound_port)));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-UAC-Hardware/1.0".to_string()));

        // SDP: Port + 2 (RTP için)
        let sdp = format!("v=0\r\no=- 123 123 IN IP4 0.0.0.0\r\ns=SentiricHW\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", bound_port+2);
        invite.body = sdp.as_bytes().to_vec();

        // LOG: INVITE Gönderimi
        let _ = self.event_tx.send(UacEvent::Log(format!("-> Sending INVITE to {}", target_addr))).await;
        let _ = self.event_tx.send(UacEvent::Status("Dialing...".into())).await;
        
        socket.send_to(&invite.to_bytes(), target_addr).await?;

        let mut buf = [0u8; 4096];
        loop {
            let (size, src) = socket.recv_from(&mut buf).await?;
            let packet = match parser::parse(&buf[..size]) {
                Ok(p) => p,
                Err(e) => {
                    let _ = self.event_tx.send(UacEvent::Log(format!("Parse Error: {:?}", e))).await;
                    continue;
                },
            };

            // LOG: Gelen SIP Paketi
            if !packet.is_request {
                let _ = self.event_tx.send(UacEvent::Log(format!("<- Received SIP {} {}", packet.status_code, packet.reason))).await;
            }

            if packet.status_code == 200 {
                let _ = self.event_tx.send(UacEvent::Status("CONNECTED (Hardware Active)".into())).await;
                
                // ACK
                let remote_tag = packet.get_header_value(HeaderName::To).cloned().unwrap_or(to.clone());
                let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                ack.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
                ack.headers.push(Header::new(HeaderName::From, from.clone()));
                ack.headers.push(Header::new(HeaderName::To, remote_tag));
                ack.headers.push(Header::new(HeaderName::CSeq, "1 ACK".to_string()));
                ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-ack", bound_port)));
                
                socket.send_to(&ack.to_bytes(), target_addr).await?;
                let _ = self.event_tx.send(UacEvent::Log("-> Sending ACK".into())).await;

                // Soketi standart senkron sokete çevir
                let std_socket = socket.into_std()?;
                std_socket.set_nonblocking(false)?; 
                
                // Hedef Port: Basitlik için SBC'nin beklediği portu manuel hedefliyoruz.
                // İdealde SDP'den parse edilmeli. Şimdilik loglardan gördüğümüz 30004 portu.
                // Veya SBC'nin NAT fix ile düzelttiği IP'ye geri dönmeli.
                let rtp_target = SocketAddr::new(target_addr.ip(), 30004); 
                
                let event_tx_clone = self.event_tx.clone();

                // Ses döngüsünü ayrı bir OS thread'inde başlat
                std::thread::spawn(move || {
                    if let Err(e) = Self::run_hardware_audio_stream_sync(std_socket, rtp_target, event_tx_clone.clone()) {
                        let _ = event_tx_clone.blocking_send(UacEvent::Error(format!("Audio Fail: {}", e)));
                    }
                    let _ = event_tx_clone.blocking_send(UacEvent::CallEnded);
                });
                
                break;
            }
        }
        Ok(())
    }

    /// [SENKRON] Ses işleme döngüsü.
    fn run_hardware_audio_stream_sync(socket: StdUdpSocket, target: SocketAddr, event_tx: mpsc::Sender<UacEvent>) -> anyhow::Result<()> {
        let host = cpal::default_host();
        let input_device = host.default_input_device().ok_or_else(|| anyhow::anyhow!("Mic not found"))?;
        let output_device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("Speaker not found"))?;

        let config: cpal::StreamConfig = input_device.default_input_config()?.into();
        let sample_rate = config.sample_rate.0 as usize;
        let _ = event_tx.blocking_send(UacEvent::Log(format!("Hardware Rate: {}Hz", sample_rate)));

        // Mikrofon -> RingBuffer
        let rb = HeapRb::<f32>::new(sample_rate * 2);
        let (mut mic_prod, mut mic_cons) = rb.split();

        let _input_stream = input_device.build_input_stream(
            &config,
            move |data: &[f32], _| { for &sample in data { let _ = mic_prod.push(sample); } },
            |err| eprintln!("Mic err: {}", err),
            None
        )?;

        // RingBuffer -> Hoparlör
        let out_rb = HeapRb::<f32>::new(sample_rate * 2);
        let (mut spk_prod, mut spk_cons) = out_rb.split();

        let _output_stream = output_device.build_output_stream(
            &config,
            move |data: &mut [f32], _| { for sample in data.iter_mut() { *sample = spk_cons.pop().unwrap_or(0.0); } },
            |err| eprintln!("Spk err: {}", err),
            None
        )?;

        _input_stream.play()?;
        _output_stream.play()?;

        // Codec ve Pacer
        let profile = AudioProfile::default();
        let mut encoder = CodecFactory::create_encoder(CodecType::PCMU); // PCMU Zorla
        let mut decoder = CodecFactory::create_decoder(CodecType::PCMU);
        let mut pacer = Pacer::new(20);
        
        let rtp_ssrc: u32 = rand::random();
        let mut rtp_seq: u16 = 0;
        let mut rtp_ts: u32 = 0;
        let mut recv_buf = [0u8; 2048];

        let _ = event_tx.blocking_send(UacEvent::Log("Media Loop Started".into()));

        // --- TELEMETRİ SAYAÇLARI ---
        let mut tx_count = 0;
        let mut rx_count = 0;
        let mut last_log = std::time::Instant::now();

        loop {
            pacer.wait();

            // 1. TX: Mikrofondan alıp ağa gönder
            let mut mic_samples = Vec::new();
            while let Some(s) = mic_cons.pop() { 
                mic_samples.push((s * 32767.0) as i16); 
            }
            
            if !mic_samples.is_empty() {
                let resampled = simple_resample(&mic_samples, sample_rate, 8000);
                let payload = encoder.encode(&resampled);
                
                if !payload.is_empty() {
                    let pkt = RtpPacket { header: RtpHeader::new(0, rtp_seq, rtp_ts, rtp_ssrc), payload };
                    
                    if let Err(e) = socket.send_to(&pkt.to_bytes(), target) {
                         let _ = event_tx.blocking_send(UacEvent::Error(format!("UDP Send Err: {}", e)));
                         break;
                    }

                    tx_count += 1;
                    rtp_seq = rtp_seq.wrapping_add(1);
                    rtp_ts = rtp_ts.wrapping_add(160);
                }
            }

            // 2. RX: Ağdan alıp hoparlöre ver
            socket.set_nonblocking(true)?;
            if let Ok((size, src)) = socket.recv_from(&mut recv_buf) {
                // Sadece hedef veya SBC'den gelenleri işle (Güvenlik)
                if src.ip() == target.ip() { 
                    rx_count += 1;
                    if let Ok(pkt) = parser::parse(&recv_buf[..size]) {
                        // Eğer RTP Header geçerliyse işle (Basit kontrol: Version 2 = 10xxxxxx)
                        // parser::parse aslında SIP parser'dır, RTP için manual offset gerekebilir.
                        // Ancak RTP core entegre olmadığı için basitçe ilk 12 byte header'ı atlıyoruz.
                        if size > 12 {
                            let rtp_payload = &recv_buf[12..size];
                            let samples_8k = decoder.decode(rtp_payload);
                            let resampled = simple_resample(&samples_8k, 8000, sample_rate);
                            for s in resampled { 
                                let _ = spk_prod.push(s as f32 / 32768.0); 
                            }
                        }
                    }
                }
            }
            socket.set_nonblocking(false)?;

            // 3. LOGLAMA (Her 2 saniyede bir)
            if last_log.elapsed().as_secs() >= 2 {
                let _ = event_tx.blocking_send(UacEvent::Log(
                    format!("📊 RTP Stats | TX: {} pkts | RX: {} pkts | Target: {}", tx_count, rx_count, target)
                ));
                last_log = std::time::Instant::now();
            }
        }
        
        Ok(())
    }
}