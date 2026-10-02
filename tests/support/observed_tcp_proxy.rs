//! A relay standing between a Server and the Serving listener it is paired
//! with, so a test sees each connection the Pairing opens and can take the
//! route offline, or have it take connections and answer nothing, as a machine
//! that is off or that accepts and then says nothing would.

use tokio::time::timeout;

use super::PROGRESS_DEADLINE;

/// A route to one Serving listener on this machine's loopback, counting the
/// connections it opens and carries.
pub struct ObservedTcpProxy {
    pub address: std::net::SocketAddr,
    active_connections: tokio::sync::watch::Receiver<usize>,
    opened_connections: tokio::sync::watch::Receiver<usize>,
    online: tokio::sync::watch::Sender<bool>,
    hold: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ObservedTcpProxy {
    pub async fn start(target: std::net::SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind observed Pairing route");
        let address = listener.local_addr().expect("read observed Pairing route");
        let (active, active_connections) = tokio::sync::watch::channel(0_usize);
        let (opened, opened_connections) = tokio::sync::watch::channel(0_usize);
        let (online, online_rx) = tokio::sync::watch::channel(true);
        let (hold, hold_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            // A held connection is kept open and never forwarded, so a dialer
            // waits on it exactly as it would on a machine that accepts and
            // then says nothing.
            let mut held = Vec::new();
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    break;
                };
                opened.send_modify(|count| *count += 1);
                if !*online_rx.borrow() {
                    if *hold_rx.borrow() {
                        held.push(inbound);
                    }
                    continue;
                }
                active.send_modify(|count| *count += 1);
                let active = active.clone();
                let mut online = online_rx.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                        let transfer = tokio::io::copy_bidirectional(&mut inbound, &mut outbound);
                        tokio::pin!(transfer);
                        loop {
                            tokio::select! {
                                _ = &mut transfer => break,
                                changed = online.changed() => {
                                    if changed.is_err() || !*online.borrow() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    active.send_modify(|count| *count = count.saturating_sub(1));
                });
            }
        });
        Self {
            address,
            active_connections,
            opened_connections,
            online,
            hold,
            task,
        }
    }

    /// Accepts connections and answers nothing, so a dialer's own budget is
    /// the only thing that ends the attempt.
    pub async fn swallow_connections(&mut self) {
        self.hold.send_replace(true);
        self.set_online(false).await;
    }

    pub async fn set_online(&mut self, online: bool) {
        self.online.send_replace(online);
        if !online {
            self.wait_for_connections(0).await;
        }
    }

    pub fn opened_connections(&self) -> usize {
        *self.opened_connections.borrow()
    }

    pub async fn wait_for_opened_connections(&mut self, expected: usize) {
        wait_for_counter(
            &mut self.opened_connections,
            expected,
            |actual, expected| actual >= expected,
            "open at least",
        )
        .await;
    }

    pub async fn wait_for_connections(&mut self, expected: usize) {
        wait_for_counter(
            &mut self.active_connections,
            expected,
            |actual, expected| actual == expected,
            "settle at",
        )
        .await;
    }
}

async fn wait_for_counter(
    counter: &mut tokio::sync::watch::Receiver<usize>,
    expected: usize,
    reached: fn(usize, usize) -> bool,
    description: &str,
) {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if reached(*counter.borrow(), expected) {
                return;
            }
            counter
                .changed()
                .await
                .expect("observed Pairing route remains open");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Pairing route should {description} {expected} connections"));
}

impl Drop for ObservedTcpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
