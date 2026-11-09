// SPDX-License-Identifier: GPL-3.0-or-later

//! A TCP proxy between a connector and a server that can stop forwarding: a pulled cable or a server
//! gone, as the client sees them. The same as `drivemount-sftp`'s test proxy.

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    Forward,
    /// Holds all data back, keeping TCP open: a pulled cable, as the client sees it.
    Stall,
    /// Closes every connection, and each new one at once: a server that has gone.
    Cut,
}

pub struct Proxy {
    pub port: u16,
    link: watch::Sender<Link>,
}

impl Proxy {
    /// Starts on the current runtime, forwarding to `server_port` on this machine.
    pub async fn to(server_port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (link, state) = watch::channel(Link::Forward);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                if *state.borrow() == Link::Cut {
                    continue;
                }
                let Ok(server) = TcpStream::connect(("127.0.0.1", server_port)).await else {
                    continue;
                };
                let (from_client, to_client) = client.into_split();
                let (from_server, to_server) = server.into_split();
                tokio::spawn(forward(from_client, to_server, state.clone()));
                tokio::spawn(forward(from_server, to_client, state.clone()));
            }
        });
        Self { port, link }
    }

    pub fn set(&self, state: Link) {
        self.link.send_replace(state);
    }
}

async fn forward(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    state: watch::Receiver<Link>,
) {
    let mut cut = state.clone();
    let copy = async move {
        let mut state = state;
        let mut buffer = vec![0; 64 * 1024];
        while let Ok(count @ 1..) = from.read(&mut buffer).await {
            let forwarding = state
                .wait_for(|link| *link != Link::Stall)
                .await
                .is_ok_and(|link| *link == Link::Forward);
            if !forwarding {
                return;
            }
            if to.write_all(&buffer[..count]).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        () = copy => {}
        _ = cut.wait_for(|link| *link == Link::Cut) => {}
    }
}
