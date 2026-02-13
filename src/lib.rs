// sentiric-telecom-client-sdk/src/lib.rs

pub mod engine;
pub mod rtp_engine;
pub mod utils;

use tokio::sync::mpsc;
use crate::engine::SipEngine;

/// UI tarafına (Flutter/CLI) gönderilecek olaylar.
#[derive(Debug, Clone)]
pub enum UacEvent {
    Log(String),
    CallStateChanged(CallState),
    Error(String),
}

/// Çağrı Durumları (State Machine)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Dialing,
    Ringing,
    Connected,
    Terminated,
}

pub struct TelecomClient {
    command_tx: mpsc::Sender<ClientCommand>,
}

/// İç motor komutları
pub enum ClientCommand {
    StartCall {
        target_ip: String,
        target_port: u16,
        to_user: String,
        from_user: String,
    },
    EndCall,
}

impl TelecomClient {
    pub fn new(event_tx: mpsc::Sender<UacEvent>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        
        // Motoru arka planda başlat
        tokio::spawn(async move {
            let mut engine = SipEngine::new(event_tx, cmd_rx).await;
            engine.run().await;
        });

        Self {
            command_tx: cmd_tx,
        }
    }

    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::StartCall { 
            target_ip, target_port, to_user, from_user 
        }).await.map_err(|_| anyhow::anyhow!("Engine is dead"))?;
        Ok(())
    }

    pub async fn end_call(&self) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::EndCall).await
            .map_err(|_| anyhow::anyhow!("Engine is dead"))?;
        Ok(())
    }
}