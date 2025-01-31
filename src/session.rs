use std::{
    collections::{hash_map::Entry, HashMap},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use byte_string::ByteStr;
use kcp::KcpResult;
use log::{error, trace};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Mutex},
    time::{self, Instant},
};

use crate::{skcp::KcpSocket, KcpConfig};

pub struct KcpSession {
    socket: Mutex<KcpSocket>,
    closed: AtomicBool,
    session_expire: Duration,
    session_close_notifier: Option<(mpsc::Sender<SocketAddr>, SocketAddr)>,
    input_tx: mpsc::Sender<Vec<u8>>,
}

impl KcpSession {
    fn new(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<(mpsc::Sender<SocketAddr>, SocketAddr)>,
        input_tx: mpsc::Sender<Vec<u8>>,
    ) -> KcpSession {
        KcpSession {
            socket: Mutex::new(socket),
            closed: AtomicBool::new(false),
            session_expire,
            session_close_notifier,
            input_tx,
        }
    }

    pub fn new_shared(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<(mpsc::Sender<SocketAddr>, SocketAddr)>,
    ) -> Arc<KcpSession> {
        let is_client = session_close_notifier.is_none();

        let (input_tx, mut input_rx) = mpsc::channel(64);

        let udp_socket = socket.udp_socket().clone();

        let session = Arc::new(KcpSession::new(
            socket,
            session_expire,
            session_close_notifier,
            input_tx,
        ));

        {
            let session = session.clone();
            tokio::spawn(async move {
                let mut input_buffer = [0u8; 65536];
                let update_timer = time::sleep(Duration::from_millis(10));
                tokio::pin!(update_timer);

                loop {
                    tokio::select! {
                        // recv() then input()
                        // Drives the KCP machine forward
                        recv_result = udp_socket.recv(&mut input_buffer), if is_client => {
                            match recv_result {
                                Err(err) => {
                                    error!("[SESSION] UDP recv failed, error: {}", err);
                                }
                                Ok(n) => {
                                    let input_buffer = &input_buffer[..n];
                                    let input_conv = kcp::get_conv(input_buffer);
                                    trace!("[SESSION] UDP recv {} bytes, conv: {}, going to input {:?}", n, input_conv, ByteStr::new(input_buffer));

                                    let mut socket = session.socket.lock().await;

                                    // Server may allocate another conv for this client.
                                    if !socket.waiting_conv() && socket.conv() != input_conv {
                                        trace!("[SESSION] UDP input conv: {} replaces session conv: {}", input_conv, socket.conv());
                                        socket.set_conv(input_conv);
                                    }

                                    match socket.input(input_buffer) {
                                        Ok(true) => {
                                            trace!("[SESSION] UDP input {} bytes and waked sender/receiver", n);
                                        }
                                        Ok(false) => {}
                                        Err(err) => {
                                            error!("[SESSION] UDP input {} bytes error: {}, input buffer {:?}", n, err, ByteStr::new(input_buffer));
                                        }
                                    }
                                }
                            }
                        }

                        // bytes received from listener socket
                        input_opt = input_rx.recv() => {
                            if let Some(input_buffer) = input_opt {
                                let mut socket = session.socket.lock().await;
                                match socket.input(&input_buffer) {
                                    Ok(..) => {
                                        trace!("[SESSION] UDP input {} bytes from channel {:?}", input_buffer.len(), ByteStr::new(&input_buffer));
                                    }
                                    Err(err) => {
                                        error!("[SESSION] UDP input {} bytes from channel failed, error: {}, input buffer {:?}",
                                               input_buffer.len(), err, ByteStr::new(&input_buffer));
                                    }
                                }
                            }
                        }

                        // Call update() in period
                        _ = &mut update_timer => {
                            let mut socket = session.socket.lock().await;

                            let is_closed = session.closed.load(Ordering::Acquire);
                            if is_closed && socket.can_close() {
                                trace!("[SESSION] KCP session closed");
                                break;
                            }

                            // server socket expires
                            if !is_client {
                                // If this is a server stream, close it automatically after a period of time
                                let last_update_time = socket.last_update_time();
                                let elapsed = last_update_time.elapsed();

                                if elapsed > session.session_expire {
                                    if elapsed > session.session_expire * 2 {
                                        // Force close. Client may have already gone.
                                        trace!(
                                            "[SESSION] force close inactive session, conv: {}, last_update: {}s ago",
                                            socket.conv(),
                                            elapsed.as_secs()
                                        );
                                        break;
                                    }

                                    if !is_closed {
                                        trace!(
                                            "[SESSION] closing inactive session, conv: {}, last_update: {}s ago",
                                            socket.conv(),
                                            elapsed.as_secs()
                                        );
                                        session.closed.store(true, Ordering::Release);
                                    }
                                }
                            }

                            match socket.update() {
                                Ok(next_next) => {
                                    update_timer.as_mut().reset(Instant::from_std(next_next));
                                }
                                Err(err) => {
                                    error!("[SESSION] KCP update failed, error: {}", err);
                                    update_timer.as_mut().reset(Instant::now() + Duration::from_millis(10));
                                }
                            }
                        }
                    }
                }

                {
                    // Close the socket.
                    // Wake all pending tasks and let all send/recv return EOF

                    let mut socket = session.socket.lock().await;
                    socket.close();
                }

                if let Some((ref notifier, peer_addr)) = session.session_close_notifier {
                    let _ = notifier.send(peer_addr).await;
                }
            });
        }

        session
    }

    pub fn kcp_socket(&self) -> &Mutex<KcpSocket> {
        &self.socket
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub async fn input(&self, buf: &[u8]) {
        self.input_tx.send(buf.to_owned()).await.expect("input channel closed")
    }

    pub async fn conv(&self) -> u32 {
        let socket = self.socket.lock().await;
        socket.conv()
    }

    pub async fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.lock().await.local_addr()
    }
}

pub struct KcpSessionManager {
    sessions: HashMap<SocketAddr, Arc<KcpSession>>,
}

impl KcpSessionManager {
    pub fn new() -> KcpSessionManager {
        KcpSessionManager {
            sessions: HashMap::new(),
        }
    }

    #[inline]
    pub fn alloc_conv(&mut self) -> u32 {
        rand::random()
    }

    pub fn close_peer(&mut self, peer_addr: SocketAddr) {
        self.sessions.remove(&peer_addr);
    }

    pub async fn get_or_create(
        &mut self,
        config: &KcpConfig,
        conv: u32,
        sn: u32,
        udp: &Arc<UdpSocket>,
        peer_addr: SocketAddr,
        session_close_notifier: &mpsc::Sender<SocketAddr>,
    ) -> KcpResult<(Arc<KcpSession>, bool)> {
        match self.sessions.entry(peer_addr) {
            Entry::Occupied(mut occ) => {
                let session = occ.get();

                if sn == 0 && session.conv().await != conv {
                    // This is the first packet received from this peer.
                    // Recreate a new session for this specific client.

                    let socket = KcpSocket::new(config, conv, udp.clone(), peer_addr, config.stream)?;
                    let session = KcpSession::new_shared(
                        socket,
                        config.session_expire,
                        Some((session_close_notifier.clone(), peer_addr)),
                    );

                    let old_session = occ.insert(session.clone());
                    let old_conv = old_session.conv().await;
                    trace!(
                        "replaced session with conv: {} (old: {}), peer: {}",
                        conv,
                        old_conv,
                        peer_addr
                    );

                    Ok((session, true))
                } else {
                    Ok((session.clone(), false))
                }
            }
            Entry::Vacant(vac) => {
                let socket = KcpSocket::new(config, conv, udp.clone(), peer_addr, config.stream)?;
                let session = KcpSession::new_shared(
                    socket,
                    config.session_expire,
                    Some((session_close_notifier.clone(), peer_addr)),
                );
                trace!("created session for conv: {}, peer: {}", conv, peer_addr);
                vac.insert(session.clone());
                Ok((session, true))
            }
        }
    }
}
