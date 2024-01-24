use crate::Args;
use bincode::{Decode, Encode};
use log::{error, trace};
use std::{
    io,
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

static TX_LEN: usize = 12;

#[derive(Encode, Decode)]
struct TX {
    magic: [u8; 4],
    addr: SocketAddrV4,
    client_port: u16,
}

pub async fn run(args: Args) -> io::Result<()> {
    let ssock = UdpSocket::bind(args.listen).await?;
    let srx = Arc::new(ssock); // server rx
    let stx = srx.clone(); // server tx

    let csock = UdpSocket::bind("[::]:0").await?;
    csock.connect(args.uu_server).await?;
    let crx = Arc::new(csock); // client rx
    let ctx = crx.clone(); // client tx

    let addr_raw: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let addr = Arc::new(Mutex::new(addr_raw));

    tokio::spawn({
        let addr = addr.clone();
        async move {
            start_tx(addr, srx, ctx, &args.target, args.magic).await;
        }
    });

    start_rx(addr.clone(), crx, stx).await;
    Ok(())
}

async fn start_tx(
    addr: Arc<Mutex<SocketAddr>>,
    srx: Arc<UdpSocket>,
    ctx: Arc<UdpSocket>,
    target: &str,
    magic: [u8; 4],
) {
    // Receive packets from local client and forward it to UU server
    let mut sbuf = [0; 1500];
    let bincode_config = bincode::config::standard()
        .with_big_endian()
        .with_fixed_int_encoding();
    loop {
        let (len, client_addr) = srx.recv_from(&mut sbuf[TX_LEN..]).await.unwrap();
        trace!("{:?} bytes received from {:?}", len, client_addr);

        let header = TX {
            magic,
            addr: target.parse().unwrap(),
            client_port: client_addr.port(),
        };
        bincode::encode_into_slice(&header, &mut sbuf[..TX_LEN], bincode_config).unwrap();

        {
            let mut addr = addr.lock().await;
            *addr = client_addr;
        }
        let len = ctx.send(&sbuf[..len + TX_LEN]).await.unwrap();
        trace!("{:?} bytes sent", len);
    }
}

async fn start_rx(addr: Arc<Mutex<SocketAddr>>, crx: Arc<UdpSocket>, stx: Arc<UdpSocket>) {
    // Receive packets from UU server and forward it to local client
    let mut cbuf = [0; 1500];
    loop {
        let len = match crx.recv(&mut cbuf).await {
            Ok(len) => len,
            Err(err) => {
                error!("client recv error: {}", err);
                continue;
            }
        };
        trace!("{:?} bytes received", len);
        let addr = {
            let addr = addr.lock().await;
            *addr
        };
        let _ = stx.send_to(&cbuf[10..len], addr).await;
    }
}
