// sentiric-telecom-client-sdk/src/engine.rs

use crate::{CallState, ClientCommand, UacEvent};
use crate::rtp_engine::RtpEngine;
use crate::utils::extract_rtp_target;
use sentiric_sip_core::{parser, Header, HeaderName, Method, SipPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{info, warn};
use sentiric_sip_core::utils::extract_socket_addr; // Adres çözücü helper

pub struct SipEngine {
    event_tx: mpsc::Sender<UacEvent>,
    command_rx: mpsc::Receiver<ClientCommand>,
    rtp_engine: Option<RtpEngine>,
    state: CallState,
}

impl SipEngine {
    pub async fn new(
        event_tx: mpsc::Sender<UacEvent>,
        command_rx: mpsc::Receiver<ClientCommand>,
    ) -> Self {
        Self {
            event_tx,
            command_rx,
            rtp_engine: None,
            state: CallState::Idle,
        }
    }

    /// Durum değişikliğini hem loglar hem de UI'a bildirir.
    fn change_state(&mut self, new_state: CallState) {
        if self.state != new_state {
            info!("State Transition: {:?} -> {:?}", self.state, new_state);
            self.state = new_state.clone();
            // Kanal doluysa veya kapalıysa uygulamayı çökertme, logla geç.
            if let Err(e) = self.event_tx.try_send(UacEvent::CallStateChanged(new_state)) {
                warn!("Failed to send state change event to UI: {}", e);
            }
        }
    }

    /// UI/CLI tarafına log gönderir.
    async fn send_ui_log(&self, msg: String) {
        if let Err(_) = self.event_tx.send(UacEvent::Log(msg)).await {
            warn!("UI event channel closed, cannot send log.");
        }
    }

    /// UI/CLI tarafına hata bildirir.
    async fn send_ui_error(&self, msg: String) {
        warn!("SDK Error: {}", msg); // Sistem loguna da bas
        if let Err(_) = self.event_tx.send(UacEvent::Error(msg)).await {
            warn!("UI event channel closed, cannot send error.");
        }
    }

    pub async fn run(&mut self) {
        // OS'in boş bir port atamasına izin ver (0.0.0.0:0)
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                self.send_ui_error(format!("Fatal: UDP Bind Failed: {}", e)).await;
                return;
            }
        };

        // RTP Motorunu başlat
        self.rtp_engine = Some(RtpEngine::new(socket.clone()));

        let mut buf = [0u8; 4096];

        // Çağrı State Verileri
        let mut current_target: Option<SocketAddr> = None;
        let mut current_call_id = String::new();
        let mut current_from_tag = String::new();
        let mut current_to_tag = String::new();
        let mut current_cseq = 0;

        // Retransmission (RFC 3261 Timer A benzeri)
        let mut last_invite_packet: Option<Vec<u8>> = None;
        // 500ms'de bir kontrol et
        let mut retransmit_interval = tokio::time::interval(Duration::from_millis(500));
        // Interval'in hemen tetiklenmesini önle (Tick strategy)
        retransmit_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        
        let mut invite_sent_time: Option<std::time::Instant> = None;

        info!("SIP Engine Loop Started on local port: {:?}", socket.local_addr());

        loop {
            tokio::select! {
                // -----------------------------------------------------------
                // 1. UI/CLI'dan Gelen Komutlar
                // -----------------------------------------------------------
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        ClientCommand::StartCall { target_ip, target_port, to_user, from_user } => {
                            if self.state != CallState::Idle {
                                self.send_ui_error("Call already in progress".into()).await;
                                continue;
                            }

                            // Hedef Adresi Çöz
                            let target_str = format!("{}:{}", target_ip, target_port);
                            let target_addr: SocketAddr = match target_str.parse() {
                                Ok(a) => a,
                                Err(e) => {
                                    self.send_ui_error(format!("Invalid Target Address: {}", e)).await;
                                    continue;
                                }
                            };

                            current_target = Some(target_addr);
                            current_call_id = format!("uac-{:x}", rand::random::<u32>());
                            current_from_tag = format!("tag-{:x}", rand::random::<u16>());
                            current_cseq = 1;

                            let bound_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);

                            // SIP INVITE Oluşturma
                            let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}@{}:{}", to_user, target_ip, target_port));
                            let branch = sentiric_sip_core::utils::generate_branch_id();

                            invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch={}", bound_port, branch)));
                            invite.headers.push(Header::new(HeaderName::From, format!("<sip:{}@sentiric.mobile>;tag={}", from_user, current_from_tag)));
                            invite.headers.push(Header::new(HeaderName::To, format!("<sip:{}@{}>", to_user, target_ip)));
                            invite.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                            invite.headers.push(Header::new(HeaderName::CSeq, format!("{} INVITE", current_cseq)));
                            invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@0.0.0.0:{}>", from_user, bound_port)));
                            invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                            invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-Telecom-SDK/2.1".to_string()));
                            invite.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));

                            // SDP: PCMU (0) ve Telephone-Event (101)
                            let sdp = format!(
                                "v=0\r\n\
                                 o=- 1 1 IN IP4 0.0.0.0\r\n\
                                 s=Sentiric\r\n\
                                 c=IN IP4 0.0.0.0\r\n\
                                 t=0 0\r\n\
                                 m=audio {} RTP/AVP 0 101\r\n\
                                 a=rtpmap:0 PCMU/8000\r\n\
                                 a=rtpmap:101 telephone-event/8000\r\n\
                                 a=sendrecv\r\n\
                                 a=ptime:20\r\n",
                                bound_port
                            );
                            invite.body = sdp.as_bytes().to_vec();

                            let packet_bytes = invite.to_bytes();
                            
                            // Loglama
                            let raw_log = String::from_utf8_lossy(&packet_bytes).to_string();
                            self.send_ui_log(format!("📤 OUTGOING INVITE ({} bytes):\n{}", packet_bytes.len(), raw_log)).await;

                            // Gönderim
                            if let Err(e) = socket.send_to(&packet_bytes, target_addr).await {
                                self.send_ui_error(format!("Socket Send Error: {}", e)).await;
                                continue;
                            }

                            // Retransmission Hazırlığı
                            last_invite_packet = Some(packet_bytes);
                            invite_sent_time = Some(std::time::Instant::now());
                            retransmit_interval.reset();

                            self.change_state(CallState::Dialing);
                        },
                        ClientCommand::EndCall => {
                             last_invite_packet = None; // Tekrar gönderimi durdur
                             
                             if self.state != CallState::Idle {
                                if let Some(target) = current_target {
                                    current_cseq += 1;
                                    let mut bye = SipPacket::new_request(Method::Bye, format!("sip:{}", target));
                                    // BYE için minimum headerlar
                                    let branch = sentiric_sip_core::utils::generate_branch_id();
                                    bye.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:0;branch={}", branch)));
                                    bye.headers.push(Header::new(HeaderName::From, format!("<sip:mobile@sentiric>;tag={}", current_from_tag)));
                                    bye.headers.push(Header::new(HeaderName::To, format!("<sip:server>;tag={}", current_to_tag))); 
                                    bye.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                                    bye.headers.push(Header::new(HeaderName::CSeq, format!("{} BYE", current_cseq)));
                                    bye.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));

                                    let _ = socket.send_to(&bye.to_bytes(), target).await;
                                    self.send_ui_log("📤 BYE Sent".into()).await;
                                }
                                
                                if let Some(rtp) = &self.rtp_engine { rtp.stop(); }
                                self.change_state(CallState::Terminated);
                                self.change_state(CallState::Idle);
                            }
                        }
                    }
                }, // 1. Kol Sonu

                // -----------------------------------------------------------
                // 2. Ağdan Gelen Paketler (SIP)
                // -----------------------------------------------------------
                Ok((size, src)) = socket.recv_from(&mut buf) => {
                    // Keep-alive veya bozuk paket filtresi
                    if size < 4 || (buf[0] & 0x80) != 0 {
                         continue;
                    }

                    // Ham loglama
                    let raw_in = String::from_utf8_lossy(&buf[..size]).to_string();
                    self.send_ui_log(format!("📥 INCOMING from {}:\n{}", src, raw_in)).await;

                    if let Ok(packet) = parser::parse(&buf[..size]) {
                         
                         // Cevap geldiyse Retransmission'ı durdur (1xx, 2xx, 3xx...)
                         if packet.is_response() && packet.status_code >= 100 {
                             if last_invite_packet.is_some() {
                                 last_invite_packet = None; 
                                 info!("ACK Loop Stopped: Response {} received.", packet.status_code);
                             }
                         }

                         // 180 Ringing
                         if packet.is_response() && packet.status_code == 180 {
                             self.change_state(CallState::Ringing);
                         }

                        // [KRİTİK GÜNCELLEME: 200 OK İşleme Bloğu]
                        if packet.is_response() && packet.status_code == 200 && (self.state == CallState::Dialing || self.state == CallState::Ringing) {
                            
                            // 1. To Tag'i sakla (BYE için gerekli)
                            if let Some(to) = packet.get_header_value(HeaderName::To) {
                                current_to_tag = to.clone(); 
                            }

                            // 2. ACK HEDEFİNİ BELİRLE (RFC 3261 Uyumlu)
                            // Önce Contact header'ına bak, yoksa paketin geldiği adrese fallback yap.
                            let ack_target = if let Some(contact) = packet.get_header_value(HeaderName::Contact) {
                                if let Some(addr) = extract_socket_addr(contact) {
                                    info!("🎯 ACK Target from Contact Header: {}", addr);
                                    addr
                                } else {
                                    src // Fallback to packet source
                                }
                            } else {
                                src
                            };

                            // 3. RTP Latching (SDP İşleme)
                            let sip_ip = src.ip().to_string();
                            if let Some(rtp_target) = extract_rtp_target(&packet.body, &sip_ip) {
                                info!("🎙️ RTP Target: {}", rtp_target);
                                if let Some(rtp) = &self.rtp_engine {
                                    rtp.start(rtp_target);
                                }
                            }

                            // 4. ACK Paketini İnşa Et
                            // ACK URI'si standarda göre Request-URI ile aynı olmalı veya Contact URI olmalı.
                            let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", ack_target));
                            let branch = sentiric_sip_core::utils::generate_branch_id();
                            let bound_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);

                            ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch={}", bound_port, branch)));
                            ack.headers.push(Header::new(HeaderName::From, format!("<sip:mobile@sentiric>;tag={}", current_from_tag)));
                            ack.headers.push(Header::new(HeaderName::To, current_to_tag.clone()));
                            ack.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                            ack.headers.push(Header::new(HeaderName::CSeq, format!("{} ACK", current_cseq)));
                            ack.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));
                            
                            // 5. ACK GÖNDERİMİ
                            if let Err(e) = socket.send_to(&ack.to_bytes(), ack_target).await {
                                warn!("❌ Failed to send ACK to {}: {}", ack_target, e);
                            } else {
                                self.send_ui_log(format!("--> ACK Sent to {}", ack_target)).await;
                            }
                            
                            self.change_state(CallState::Connected);
                        }
                         
                         // Karşı taraf BYE gönderirse
                         if packet.is_request() && packet.method == Method::Bye {
                             self.send_ui_log("📥 BYE Received from Server".into()).await;
                             // 200 OK Dön
                             let ok = sentiric_sip_core::SipResponseFactory::create_200_ok(&packet);
                             let _ = socket.send_to(&ok.to_bytes(), src).await;
                             
                             if let Some(rtp) = &self.rtp_engine { rtp.stop(); }
                             self.change_state(CallState::Terminated);
                             self.change_state(CallState::Idle);
                         }
                    } // Parser if
                }, // 2. Kol Sonu

                // -----------------------------------------------------------
                // 3. Retransmission Timer (RFC 3261 Timer A & B)
                // -----------------------------------------------------------
                _ = retransmit_interval.tick() => {
                    if let Some(packet) = &last_invite_packet {
                        if let Some(target) = current_target {
                            if let Some(start_time) = invite_sent_time {
                                let elapsed = start_time.elapsed();
                                if elapsed > Duration::from_secs(5) {
                                    // 5 saniye geçti ve hala cevap yok -> TIMEOUT
                                    self.send_ui_error("❌ SBC Timeout (No response in 5s). Check Firewall/Network.".into()).await;
                                    last_invite_packet = None;
                                    self.change_state(CallState::Terminated);
                                    self.change_state(CallState::Idle);
                                } else {
                                    // Retransmit (Tekrar Gönder)
                                    self.send_ui_log(format!("🔄 Retransmitting INVITE ({}s elapsed)...", elapsed.as_secs())).await;
                                    if let Err(e) = socket.send_to(packet, target).await {
                                         warn!("Retransmit failed: {}", e);
                                    }
                                }
                            }
                        }
                    }
                } // 3. Kol Sonu
            } // Select Sonu
        } // Loop Sonu
    } // Run Sonu
} // Impl Sonu