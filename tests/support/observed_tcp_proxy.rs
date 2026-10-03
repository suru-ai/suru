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
    target: tokio::sync::watch::Sender<std::net::SocketAddr>,
    active_connections: tokio::sync::watch::Receiver<usize>,
    opened_connections: tokio::sync::watch::Receiver<usize>,
    online: tokio::sync::watch::Sender<bool>,
    hold: tokio::sync::watch::Sender<bool>,
    stalled: tokio::sync::watch::Sender<bool>,
    losing: tokio::sync::watch::Sender<bool>,
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
        let (stalled, stalled_rx) = tokio::sync::watch::channel(false);
        let (losing, losing_rx) = tokio::sync::watch::channel(false);
        let (target, target_rx) = tokio::sync::watch::channel(target);
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
                if *stalled_rx.borrow() {
                    held.push(inbound);
                    continue;
                }
                if !*online_rx.borrow() {
                    if *hold_rx.borrow() {
                        held.push(inbound);
                    }
                    continue;
                }
                active.send_modify(|count| *count += 1);
                let active = active.clone();
                let mut online = online_rx.clone();
                let mut stalled = stalled_rx.clone();
                let losing = losing_rx.clone();
                let target = *target_rx.borrow();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                        let mut stall = false;
                        {
                            let transfer = carry(&mut inbound, &mut outbound, losing);
                            tokio::pin!(transfer);
                            loop {
                                tokio::select! {
                                    _ = &mut transfer => break,
                                    changed = online.changed() => {
                                        if changed.is_err() || !*online.borrow() {
                                            break;
                                        }
                                    }
                                    changed = stalled.changed() => {
                                        if changed.is_ok() && *stalled.borrow() {
                                            stall = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        // A stalled connection stays open and carries
                        // nothing until the stall ends or the route goes
                        // offline.
                        while stall && *online.borrow() && *stalled.borrow() {
                            tokio::select! {
                                changed = online.changed() => if changed.is_err() { break },
                                changed = stalled.changed() => if changed.is_err() { break },
                            }
                        }
                    }
                    active.send_modify(|count| *count = count.saturating_sub(1));
                });
            }
        });
        Self {
            address,
            target,
            active_connections,
            opened_connections,
            online,
            hold,
            stalled,
            losing,
            task,
        }
    }

    /// Carries every connection opened from now on to `target` instead, as a
    /// machine that came back at another address behind the same name would
    /// be reached.
    pub fn retarget(&self, target: std::net::SocketAddr) {
        self.target.send_replace(target);
    }

    /// Keeps every connection open and carries nothing more over it, as a
    /// machine that falls silent mid-conversation would, and takes new ones
    /// and answers nothing. Going offline then online again ends the stall.
    pub async fn stall(&mut self) {
        self.stalled.send_replace(true);
    }

    /// Accepts connections and answers nothing, so a dialer's own budget is
    /// the only thing that ends the attempt.
    pub async fn swallow_connections(&mut self) {
        self.hold.send_replace(true);
        self.set_online(false).await;
    }

    /// Carries what is asked over every connection, open or new, and loses
    /// every answer on its way back, as a machine whose replies stop getting
    /// through would — so what is asked is done there, and nothing of it is
    /// heard. Going online again ends it.
    pub fn lose_answers(&mut self) {
        self.losing.send_replace(true);
    }

    pub async fn set_online(&mut self, online: bool) {
        if online {
            self.stalled.send_replace(false);
            self.hold.send_replace(false);
            self.losing.send_replace(false);
        }
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

    /// Waits until at least `expected` connections are open over the route
    /// at once.
    pub async fn wait_for_connections_at_least(&mut self, expected: usize) {
        wait_for_counter(
            &mut self.active_connections,
            expected,
            |actual, expected| actual >= expected,
            "hold at least",
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

/// Carries bytes both ways between `inbound` and `outbound` until either
/// side ends, dropping what `outbound` answers while `losing` holds.
async fn carry(
    inbound: &mut tokio::net::TcpStream,
    outbound: &mut tokio::net::TcpStream,
    losing: tokio::sync::watch::Receiver<bool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut asked, mut answered_to) = inbound.split();
    let (mut answering, mut asked_of) = outbound.split();
    // What is asked ending ends no answer still on its way: the far side is
    // told nothing more is coming, and the answers carry on until it ends.
    let up = async {
        let _ = tokio::io::copy(&mut asked, &mut asked_of).await;
        let _ = asked_of.shutdown().await;
        std::future::pending::<()>().await;
    };
    let down = async {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            let read = answering.read(&mut buffer).await?;
            if read == 0 {
                return Ok::<_, std::io::Error>(());
            }
            if !*losing.borrow() {
                answered_to.write_all(&buffer[..read]).await?;
            }
        }
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
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
