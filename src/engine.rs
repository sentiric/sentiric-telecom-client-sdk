// sentiric-telecom-client-sdk/src/engine.rs

use crate::{UacEvent, CallState, ClientCommand};
use crate::rtp_engine::RtpEngine;
use crate::utils::extract_rtp_target;
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, parser};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use std::sync::Arc;
use rand::Rng;
use std::net::SocketAddr;
use tracing::{info, error};

pub struct SipEngine {
    event_tx: mpsc::Sender<UacEvent>,
    command_rx: mpsc::Receiver<ClientCommand>,
    rtp_engine: Option<RtpEngine>,
    state: CallState,
}

impl SipEngine {
    pub async fn new(event_tx: mpsc::Sender<UacEvent>, command_rx: mpsc::Receiver<ClientCommand>) -> Self {
        Self {
            event_tx,
            command_rx,
            rtp_engine: None,
            state: CallState::Idle,
        }
    }

    fn change_state(&mut self, new_state: CallState) {
        if self.state != new_state {
            info!("State Changed: {:?} -> {:?}", self.state, new_state);
            self.state = new_state.clone();
            let _ = self.event_tx.try_send(UacEvent::CallStateChanged(new_state));
        }
    }

    pub async fn run(&mut self) {
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = self.event_tx.send(UacEvent::Error(format!("Bind Fail: {}", e))).await;
                return;
            }
        };
        
        self.rtp_engine = Some(RtpEngine::new(socket.clone()));
        
        let mut buf = [0u8; 4096];
        
        let mut current_target: Option<SocketAddr> = None;
        let mut current_call_id = String::new();
        let mut current_from_tag = String::new();
        let mut current_to_tag = String::new();
        let mut current_cseq = 0;

        loop {
            tokio::select! {
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        ClientCommand::StartCall { target_ip, target_port, to_user, from_user } => {
                            if self.state != CallState::Idle { continue; }
                            
                            let target_addr: SocketAddr = format!("{}:{}", target_ip, target_port).parse().unwrap();
                            current_target = Some(target_addr);
                            current_call_id = format!("uac-{}", rand::thread_rng().gen::<u32>());
                            current_from_tag = format!("tag-{}", rand::thread_rng().gen::<u16>());
                            current_cseq = 1;
                            
                            let bound_port = socket.local_addr().unwrap().port();
                            
                            let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}@{}:{}", to_user, target_ip, target_port));
                            
                            let branch = sentiric_sip_core::utils::generate_branch_id();
                            invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch={}", bound_port, branch)));
                            invite.headers.push(Header::new(HeaderName::From, format!("<sip:{}@sentiric.mobile>;tag={}", from_user, current_from_tag)));
                            invite.headers.push(Header::new(HeaderName::To, format!("<sip:{}@{}>", to_user, target_ip)));
                            invite.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                            invite.headers.push(Header::new(HeaderName::CSeq, format!("{} INVITE", current_cseq)));
                            invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@0.0.0.0:{}>", from_user, bound_port)));
                            invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                            invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-Telecom-SDK/1.0".to_string()));

                            let sdp = format!("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=Sentiric\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", bound_port);
                            invite.body = sdp.as_bytes().to_vec();

                            let _ = socket.send_to(&invite.to_bytes(), target_addr).await;
                            self.change_state(CallState::Dialing);
                            let _ = self.event_tx.send(UacEvent::Log("--> INVITE Sent".into())).await;
                        },
                        ClientCommand::EndCall => {
                            if self.state != CallState::Idle {
                                if let Some(target) = current_target {
                                    current_cseq += 1;
                                    // [FIX] 'mut' kaldırıldı.
                                    let bye = SipPacket::new_request(Method::Bye, format!("sip:{}", target));
                                    // Headerlar eklenecek...
                                    let _ = socket.send_to(&bye.to_bytes(), target).await;
                                }
                                if let Some(rtp) = &self.rtp_engine { rtp.stop(); }
                                self.change_state(CallState::Terminated);
                                // Reset after a while
                                self.change_state(CallState::Idle);
                            }
                        }
                    }
                },

                Ok((size, src)) = socket.recv_from(&mut buf) => {
                    if size < 4 || (buf[0] & 0x80) != 0 {
                         continue;
                    }

                    if let Ok(packet) = parser::parse(&buf[..size]) {
                         let _ = self.event_tx.send(UacEvent::Log(format!("<-- {} {}", packet.status_code, packet.reason))).await;

                         if packet.is_response() && packet.status_code == 200 && self.state == CallState::Dialing {
                             
                             if let Some(to) = packet.get_header_value(HeaderName::To) {
                                 current_to_tag = to.clone(); 
                             }

                             let sip_ip = src.ip().to_string();
                             if let Some(rtp_target) = extract_rtp_target(&packet.body, &sip_ip) {
                                 info!("RTP Target Detected: {}", rtp_target);
                                 if let Some(rtp) = &self.rtp_engine {
                                     rtp.start(rtp_target);
                                 }
                             } else {
                                 error!("SDP Parse Error! No RTP target found.");
                             }

                             let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                             let branch = sentiric_sip_core::utils::generate_branch_id();
                             let bound_port = socket.local_addr().unwrap().port();

                             ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch={}", bound_port, branch)));
                             ack.headers.push(Header::new(HeaderName::From, format!("<sip:mobile@sentiric>;tag={}", current_from_tag)));
                             ack.headers.push(Header::new(HeaderName::To, current_to_tag.clone()));
                             ack.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                             ack.headers.push(Header::new(HeaderName::CSeq, format!("{} ACK", current_cseq)));
                             
                             if let Some(target) = current_target {
                                 let _ = socket.send_to(&ack.to_bytes(), target).await;
                                 let _ = self.event_tx.send(UacEvent::Log("--> AUTO-ACK Sent".into())).await;
                             }
                             
                             self.change_state(CallState::Connected);
                         }
                    }
                }
            }
        }
    }
}