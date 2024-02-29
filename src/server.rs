use crate::Args;
use bincode::{Decode, Encode};
use log::{error, info, trace};
use std::{
    collections::{
        hash_map::Entry::{Occupied, Vacant},
        HashMap,
    },
    io,
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::{
    net::UdpSocket,
    sync::{Mutex, RwLock},
    task::AbortHandle,
    time::{sleep, Duration, Instant},
};

static TX_LEN: usize = 12;
static TIMEOUT_SEC: u64 = 300;

#[derive(Encode, Decode)]
struct TX {
    magic: [u8; 4],
    addr: SocketAddrV4,
    client_port: u16,
}

struct ServerContext {
    local_sock: Arc<UdpSocket>,
    remote_sock: UdpSocket,
    handle: Option<AbortHandle>,
}

struct Server {
    ctx: Arc<RwLock<ServerContext>>,
    last_active: Arc<RwLock<Instant>>,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    target_addr: SocketAddrV4,
    magic: [u8; 4],
}

impl Server {
    async fn new(
        local_sock: Arc<UdpSocket>,
        server_addr: SocketAddr,
        client_addr: SocketAddr,
        target_addr: SocketAddrV4,
        magic: [u8; 4],
    ) -> io::Result<Arc<Server>> {
        let remote_sock = try_connect(server_addr).await?;
        let ctx = Arc::new(RwLock::new(ServerContext {
            local_sock,
            remote_sock,
            handle: None,
        }));
        let server = Arc::new(Server {
            ctx,
            last_active: Arc::new(RwLock::new(Instant::now())),
            client_addr,
            server_addr,
            target_addr,
            magic,
        });
        let join_handle = tokio::spawn({
            let server = server.clone();
            async move { server.start_rx().await }
        });
        server.ctx.write().await.handle = Some(join_handle.abort_handle());
        Ok(server.clone())
    }

    async fn start_rx(&self) {
        // Receive packets from UU server and forward it to local client
        let mut buf = [0; 1500];
        loop {
            let ctx = self.ctx.read().await;
            let len = match ctx.remote_sock.recv(&mut buf).await {
                Ok(len) => len,
                Err(err) => {
                    error!("remote_sock recv error: {:?}", err);
                    continue;
                }
            };
            trace!("{:?} bytes received for {:?}", len, self.client_addr);
            if let Err(err) = ctx
                .local_sock
                .send_to(&buf[10..len], self.client_addr)
                .await
            {
                error!(
                    "local_sock.send_to {:?} failed: {:?}",
                    self.client_addr, err
                );
            };
            self.refresh_last_active().await;
        }
    }

    async fn process_packet(self: Arc<Self>, buf: &mut [u8], len: usize) {
        // Forward packet to UU server
        let bincode_config = bincode::config::standard()
            .with_big_endian()
            .with_fixed_int_encoding();
        let header = TX {
            magic: self.magic,
            addr: self.target_addr,
            client_port: self.client_addr.port(),
        };
        bincode::encode_into_slice(&header, &mut buf[..TX_LEN], bincode_config).unwrap();

        let len = loop {
            let result = {
                let ctx = self.ctx.read().await;
                ctx.remote_sock.send(&buf[..len + TX_LEN]).await
            };
            match result {
                Ok(len) => break len,
                Err(err) => {
                    error!(
                        "remote_sock.send for {:?} failed: {:?}, reconnecting",
                        self.client_addr, err
                    );
                    self.clone().reconnect().await;
                    continue;
                }
            }
        };
        trace!("{:?} bytes sent for {:?}", len, self.client_addr);
        self.refresh_last_active().await;
    }

    async fn reconnect(self: Arc<Self>) {
        let ctx = self.ctx.read().await;
        if let Some(handle) = ctx.handle.as_ref() {
            handle.abort();
        }
        drop(ctx);

        let mut ctx = self.ctx.write().await;
        ctx.remote_sock = try_connect(self.server_addr).await.unwrap();
        let join_handle = tokio::spawn({
            let server = self.clone();
            async move { server.start_rx().await }
        });
        ctx.handle = Some(join_handle.abort_handle());
    }

    async fn refresh_last_active(&self) {
        let now = Instant::now();
        let last_active = *self.last_active.read().await;
        if now - last_active < Duration::from_secs(10) {
            return;
        }
        *self.last_active.write().await = now;
    }

    async fn stop(&self) {
        let ctx = self.ctx.read().await;
        trace!("ctx write lock");
        if let Some(handle) = ctx.handle.as_ref() {
            handle.abort();
        };
    }
}

pub async fn run(args: Args) -> io::Result<()> {
    let local_sock = UdpSocket::bind(args.listen).await?;
    let local_sock = Arc::new(local_sock);

    let server_addr: SocketAddr = args.uu_server.parse().unwrap();
    let target_addr: SocketAddrV4 = args.target.parse().unwrap();

    start_listener(local_sock, server_addr, target_addr, args.magic).await;
    Ok(())
}

async fn start_listener(
    local_sock: Arc<UdpSocket>,
    server_addr: SocketAddr,
    target_addr: SocketAddrV4,
    magic: [u8; 4],
) {
    // Receive packets from local client
    let server_map = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let mut buf = [0; 1500];
        let (len, client_addr) = match local_sock.recv_from(&mut buf[TX_LEN..]).await {
            Ok(result) => result,
            Err(err) => {
                error!("local_sock.recv_from failed: {:?}", err);
                continue;
            }
        };
        trace!("{:?} bytes received from {:?}", len, client_addr);

        let server = match server_map.lock().await.entry(client_addr) {
            Vacant(entry) => {
                info!("Server::new for {:?}", client_addr);
                let server = match Server::new(
                    local_sock.clone(),
                    server_addr,
                    client_addr,
                    target_addr,
                    magic,
                )
                .await
                {
                    Ok(server) => server,
                    Err(err) => {
                        error!("Server::new for {:?} failed: {:?}", client_addr, err);
                        continue;
                    }
                };
                entry.insert(server.clone());
                tokio::spawn({
                    let server = server.clone();
                    let server_map = server_map.clone();
                    async move { check_expire(server_map, server, client_addr).await }
                });
                server.clone()
            }
            Occupied(entry) => entry.into_mut().clone(),
        };

        tokio::spawn(async move { server.process_packet(&mut buf, len).await });
    }
}

async fn try_connect(server_addr: SocketAddr) -> io::Result<UdpSocket> {
    let remote_sock = UdpSocket::bind("[::]:0").await?;
    loop {
        match remote_sock.connect(server_addr).await {
            Ok(_) => break,
            Err(err) => {
                error!(
                    "connect {:?} failed: {:?}, will retry after 2 seconds",
                    server_addr, err
                );
                sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Ok(remote_sock)
}

async fn check_expire(
    server_map: Arc<Mutex<HashMap<SocketAddr, Arc<Server>>>>,
    server: Arc<Server>,
    client_addr: SocketAddr,
) {
    let timeout = Duration::from_secs(TIMEOUT_SEC);
    loop {
        let delta = {
            let last_active = *server.last_active.read().await;
            tokio::time::sleep_until(last_active + timeout).await;
            Instant::now() - *server.last_active.read().await
        };
        trace!(
            "check_expire for {:?} delta = {:?}",
            client_addr,
            delta.as_secs()
        );
        if delta > timeout {
            info!("connection from {:?} expired", client_addr);
            server.stop().await;
            server_map.lock().await.remove(&client_addr);
            info!("connection from {:?} removed", client_addr);
            break;
        }
    }
}
