//! Sphaira's native Ownfoil discovery protocol (UDP broadcast port 8465).
use crate::settings::Settings;
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::UdpSocket, sync::RwLock, task::JoinHandle};

pub const PORT: u16 = 8465;
pub fn payload(settings: &Settings, http_port: u16) -> Value {
    json!({"magic":"OWNFOIL", "uid":settings.server.uid, "name":settings.shop.name,
        "version":env!("CARGO_PKG_VERSION"), "port":http_port,
        "remote":settings.shop.host, "public":settings.shop.public})
}

pub fn start(settings: Arc<RwLock<Settings>>, http: SocketAddr) -> JoinHandle<()> {
    start_on_port(settings, http, PORT)
}

fn discovery_address(http: SocketAddr, port: u16) -> SocketAddr {
    // Sphaira uses IPv4 subnet and limited broadcasts. A socket bound to a
    // specific LAN address does not receive those datagrams on macOS.
    // Preserve loopback-only operation for local development servers.
    let ip = if http.ip().is_loopback() { Ipv4Addr::LOCALHOST } else { Ipv4Addr::UNSPECIFIED };
    SocketAddr::from((ip, port))
}

fn start_on_port(settings: Arc<RwLock<Settings>>, http: SocketAddr, port: u16) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut socket = None;
        let mut buffer = [0_u8; 1024];
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            if !settings.read().await.services.discovery.enabled {
                socket = None;
                tick.tick().await;
                continue;
            }
            if socket.is_none() {
                let bind = discovery_address(http, port);
                match UdpSocket::bind(bind).await {
                    Ok(bound) => {
                        tracing::info!(%bind, "Sphaira LAN discovery listening");
                        socket = Some(bound);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "LAN discovery unavailable; manual connections still work");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                }
            }
            let Some(bound) = &socket else { continue };
            tokio::select! {
                _ = tick.tick() => {},
                request = bound.recv_from(&mut buffer) => {
                    match request {
                        Ok((length, sender)) if buffer[..length].trim_ascii() == b"OWNFOIL_DISCOVER" => {
                            let current = settings.read().await.clone();
                            if current.services.discovery.enabled {
                                let reply = payload(&current, http.port()).to_string();
                                if let Err(error) = bound.send_to(reply.as_bytes(), sender).await {
                                    tracing::debug!(%error, "Discovery reply failed");
                                }
                            }
                        }
                        Ok(_) => {},
                        Err(error) => {
                            tracing::warn!(%error, "Discovery receive failed");
                            socket = None;
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn discovery_listens_for_ipv4_broadcasts_on_lan() -> Result<()> {
        for http in ["10.0.0.89:8465", "0.0.0.0:7777", "[::]:7777"] {
            assert_eq!(discovery_address(http.parse()?, PORT), "0.0.0.0:8465".parse()?);
        }
        for http in ["127.0.0.1:7777", "[::1]:7777"] {
            assert_eq!(discovery_address(http.parse()?, PORT), "127.0.0.1:8465".parse()?);
        }
        Ok(())
    }

    async fn discover(client: &UdpSocket, server: SocketAddr) -> Result<Value> {
        let mut buffer = [0_u8; 1024];
        for _ in 0..10 {
            client.send_to(b" OWNFOIL_DISCOVER\n", server).await?;
            if let Ok(reply) =
                tokio::time::timeout(Duration::from_millis(200), client.recv_from(&mut buffer))
                    .await
            {
                let (length, _) = reply?;
                return Ok(serde_json::from_slice(&buffer[..length])?);
            }
        }
        anyhow::bail!("Discovery did not reply")
    }

    #[tokio::test]
    async fn udp_discovery_matches_identity_and_reconciles_settings() -> Result<()> {
        let reservation = UdpSocket::bind("127.0.0.1:0").await?;
        let address = reservation.local_addr()?;
        drop(reservation);
        let settings = Arc::new(RwLock::new(Settings::default()));
        let task = start_on_port(settings.clone(), "127.0.0.1:7777".parse()?, address.port());
        let client = UdpSocket::bind("127.0.0.1:0").await?;
        let reply = discover(&client, address).await?;
        assert_eq!(reply, payload(&*settings.read().await, 7777));
        assert_eq!(reply["magic"], "OWNFOIL");
        settings.write().await.shop.name = "Renamed shop".into();
        assert_eq!(discover(&client, address).await?["name"], "Renamed shop");
        client.send_to(b"unrelated broadcast", address).await?;
        let mut bytes = [0_u8; 1024];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.recv_from(&mut bytes))
                .await
                .is_err()
        );
        settings.write().await.services.discovery.enabled = false;
        client.send_to(b"OWNFOIL_DISCOVER", address).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(1100), client.recv_from(&mut bytes))
                .await
                .is_err()
        );
        settings.write().await.services.discovery.enabled = true;
        assert_eq!(discover(&client, address).await?["uid"], reply["uid"]);
        task.abort();
        let _ = task.await;
        Ok(())
    }
}
