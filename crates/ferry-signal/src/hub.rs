// Derived from LocalSend (https://github.com/localsend/localsend, Apache-2.0); modified by the Ferry authors.
//! The peer registry: connections, nearby IP groups and rooms, plus fan-out.
//!
//! Everything lives behind one `RwLock` that is held only for short,
//! synchronous critical sections (never across an `.await`). Fan-out uses
//! `try_send` into bounded per-connection queues *while the lock is held*,
//! which keeps every queue consistent with the topology (a `LEFT` can never
//! overtake the matching `JOIN`), and can never block: a peer whose queue is
//! full is told to disconnect instead.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use axum::extract::ws::Utf8Bytes;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::close;
use crate::config::Limits;
use crate::net::IpGroup;
use crate::protocol::{ClientInfo, ServerInfo, ServerMessage, encode};

/// A connection's outbound queue.
pub(crate) type Outbox = mpsc::Sender<Utf8Bytes>;

/// Internal kill code: the transport is dead, do not send a close frame.
pub(crate) const GONE: u16 = 1;

/// Shutdown signals of one connection.
pub(crate) struct ConnCtl {
    /// Immediate disconnect (`0` = alive). Watched by the reader and writer.
    kill: watch::Sender<u16>,
    /// Close code sent after the outbound queue has been flushed.
    graceful: AtomicU16,
}

impl ConnCtl {
    pub(crate) fn new() -> Self {
        Self { kill: watch::Sender::new(0), graceful: AtomicU16::new(0) }
    }

    /// Requests an immediate disconnect with `code`; the first request wins.
    pub(crate) fn kill(&self, code: u16) -> bool {
        self.kill.send_if_modified(|current| {
            if *current == 0 {
                *current = code;
                true
            } else {
                false
            }
        })
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u16> {
        self.kill.subscribe()
    }

    /// Sets the close code used once the outbound queue has drained.
    pub(crate) fn set_graceful(&self, code: u16) {
        self.graceful.store(code, Ordering::Relaxed);
    }

    /// `None`: send no close frame at all. `Some(0)`: complete the close
    /// handshake without a code. `Some(code)`: close with `code`.
    pub(crate) fn close_code(&self) -> Option<u16> {
        match *self.kill.borrow() {
            GONE => None,
            0 => Some(self.graceful.load(Ordering::Relaxed)),
            code => Some(code),
        }
    }
}

/// Resolves once a kill has been requested.
pub(crate) async fn killed(rx: &mut watch::Receiver<u16>) {
    loop {
        if *rx.borrow_and_update() != 0 {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Queues `text` without waiting; a full queue disconnects the receiver.
pub(crate) fn deliver(tx: &Outbox, ctl: &ConnCtl, text: Utf8Bytes) {
    if let Err(TrySendError::Full(_)) = tx.try_send(text) {
        ctl.kill(close::SLOW_CONSUMER);
    }
}

/// A connection about to be registered.
pub(crate) struct NewPeer {
    pub id: Uuid,
    pub info: Arc<ClientInfo>,
    pub group: IpGroup,
    pub nearby: bool,
    pub ext: bool,
    pub tx: Outbox,
    pub ctl: Arc<ConnCtl>,
}

struct PeerEntry {
    info: Arc<ClientInfo>,
    group: IpGroup,
    /// Member of its IP group (`ext.nearby != false`).
    nearby: bool,
    ext: bool,
    rooms: Vec<Arc<str>>,
    tx: Outbox,
    ctl: Arc<ConnCtl>,
}

impl PeerEntry {
    fn deliver(&self, text: &Utf8Bytes) {
        deliver(&self.tx, &self.ctl, text.clone());
    }
}

#[derive(Default)]
struct Inner {
    peers: HashMap<Uuid, PeerEntry>,
    /// Nearby members per IP group, in join order.
    groups: HashMap<IpGroup, Vec<Uuid>>,
    /// Members per room, in join order. Empty rooms are removed.
    rooms: HashMap<Arc<str>, Vec<Uuid>>,
}

#[derive(Default)]
pub(crate) struct Hub {
    inner: RwLock<Inner>,
}

pub(crate) enum RouteError {
    /// Unknown, or not visible to the sender (same answer on purpose).
    NotFound,
    /// Visible, but the target did not opt into extensions.
    Unsupported,
}

/// A relay destination resolved by [`Hub::route`].
pub(crate) struct Route {
    /// The sender's current info.
    pub sender: Arc<ClientInfo>,
    tx: Outbox,
    ctl: Arc<ConnCtl>,
}

impl Route {
    pub(crate) fn deliver(&self, text: Utf8Bytes) {
        deliver(&self.tx, &self.ctl, text);
    }
}

pub(crate) enum JoinError {
    TooManyRooms,
    RoomFull,
}

impl Hub {
    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a connection: queues its `HELLO` and sends `JOIN` to its
    /// nearby group. Returns the number of connected peers.
    pub(crate) fn register(&self, peer: NewPeer, server: Option<&ServerInfo>) -> usize {
        let join = encode(&ServerMessage::Join { peer: &peer.info });
        let mut guard = self.write();
        let Inner { peers, groups, .. } = &mut *guard;

        let hello = {
            let members: &[Uuid] = match groups.get(&peer.group) {
                Some(members) if peer.nearby => members,
                _ => &[],
            };
            encode(&ServerMessage::Hello {
                client: &peer.info,
                peers: members.iter().filter_map(|id| peers.get(id)).map(|p| &*p.info).collect(),
                server: server.filter(|_| peer.ext),
            })
        };
        deliver(&peer.tx, &peer.ctl, hello);

        if peer.nearby {
            let members = groups.entry(peer.group).or_default();
            for id in members.iter() {
                if let Some(p) = peers.get(id) {
                    p.deliver(&join);
                }
            }
            members.push(peer.id);
        }

        peers.insert(
            peer.id,
            PeerEntry {
                info: peer.info,
                group: peer.group,
                nearby: peer.nearby,
                ext: peer.ext,
                rooms: Vec::new(),
                tx: peer.tx,
                ctl: peer.ctl,
            },
        );
        peers.len()
    }

    /// Removes a connection, sending `LEFT` to its group and
    /// `ROOM_PEER_LEFT` to its rooms unless `notify` is false (server
    /// shutdown: everybody is leaving). Returns the number of connected peers.
    pub(crate) fn unregister(&self, id: Uuid, notify: bool) -> usize {
        let mut guard = self.write();
        let Inner { peers, groups, rooms } = &mut *guard;
        let Some(entry) = peers.remove(&id) else {
            return peers.len();
        };

        if entry.nearby {
            let now_empty = match groups.get_mut(&entry.group) {
                Some(members) => {
                    members.retain(|m| *m != id);
                    if notify && !members.is_empty() {
                        let left = encode(&ServerMessage::Left { peer_id: id });
                        for m in members.iter() {
                            if let Some(p) = peers.get(m) {
                                p.deliver(&left);
                            }
                        }
                    }
                    members.is_empty()
                }
                None => false,
            };
            if now_empty {
                groups.remove(&entry.group);
            }
        }

        for room in &entry.rooms {
            leave_room(peers, rooms, room, id, notify);
        }
        peers.len()
    }

    /// Stores new info for `id` and sends `UPDATE` to everyone who can see
    /// it (nearby group and room co-members).
    pub(crate) fn update(&self, id: Uuid, info: Arc<ClientInfo>) {
        let message = encode(&ServerMessage::Update { peer: &info });
        let mut guard = self.write();
        let Inner { peers, groups, rooms } = &mut *guard;
        let Some(entry) = peers.get_mut(&id) else {
            return;
        };
        entry.info = info;
        let (group, nearby, my_rooms) = (entry.group, entry.nearby, entry.rooms.clone());

        let mut recipients: Vec<Uuid> = Vec::new();
        if nearby && let Some(members) = groups.get(&group) {
            recipients.extend(members.iter().copied().filter(|m| *m != id));
        }
        for room in &my_rooms {
            for m in rooms.get(room).into_iter().flatten() {
                if *m != id && !recipients.contains(m) {
                    recipients.push(*m);
                }
            }
        }
        for m in &recipients {
            if let Some(p) = peers.get(m) {
                p.deliver(&message);
            }
        }
    }

    /// Resolves a relay from `from` to `to`. Allowed when both are nearby
    /// members of the same IP group, or share a room.
    pub(crate) fn route(&self, from: Uuid, to: Uuid, needs_ext: bool) -> Result<Route, RouteError> {
        let guard = self.read();
        let sender = guard.peers.get(&from).ok_or(RouteError::NotFound)?;
        let target = guard.peers.get(&to).filter(|_| from != to).ok_or(RouteError::NotFound)?;
        let nearby = sender.nearby && target.nearby && sender.group == target.group;
        let shared_room = sender.rooms.iter().any(|r| target.rooms.contains(r));
        if !nearby && !shared_room {
            return Err(RouteError::NotFound);
        }
        if needs_ext && !target.ext {
            return Err(RouteError::Unsupported);
        }
        Ok(Route { sender: Arc::clone(&sender.info), tx: target.tx.clone(), ctl: Arc::clone(&target.ctl) })
    }

    /// Joins `room` (already validated): `ROOM_HELLO` to the joiner and
    /// `ROOM_PEER_JOINED` to the other members. Joining a room twice just
    /// repeats the `ROOM_HELLO`.
    pub(crate) fn room_join(&self, id: Uuid, room: &str, limits: &Limits) -> Result<(), JoinError> {
        let mut guard = self.write();
        let Inner { peers, rooms, .. } = &mut *guard;
        let Some(entry) = peers.get(&id) else {
            return Ok(());
        };
        let already = entry.rooms.iter().any(|r| &**r == room);
        if !already {
            if entry.rooms.len() >= limits.max_rooms_per_conn {
                return Err(JoinError::TooManyRooms);
            }
            if rooms.get(room).is_some_and(|members| members.len() >= limits.max_room_members) {
                return Err(JoinError::RoomFull);
            }
        }

        let key: Arc<str> = rooms.get_key_value(room).map_or_else(|| Arc::from(room), |(k, _)| Arc::clone(k));
        let members = rooms.entry(Arc::clone(&key)).or_default();
        let hello = encode(&ServerMessage::RoomHello {
            room,
            peers: members.iter().filter(|m| **m != id).filter_map(|m| peers.get(m)).map(|p| &*p.info).collect(),
        });
        entry.deliver(&hello);

        if !already {
            let joined = encode(&ServerMessage::RoomPeerJoined { room, peer: &entry.info });
            for m in members.iter() {
                if let Some(p) = peers.get(m) {
                    p.deliver(&joined);
                }
            }
            members.push(id);
            if let Some(entry) = peers.get_mut(&id) {
                entry.rooms.push(key);
            }
        }
        Ok(())
    }

    /// Leaves `room`; returns `false` if `id` was not a member.
    pub(crate) fn room_leave(&self, id: Uuid, room: &str) -> bool {
        let mut guard = self.write();
        let Inner { peers, rooms, .. } = &mut *guard;
        let Some(entry) = peers.get_mut(&id) else {
            return false;
        };
        let Some(pos) = entry.rooms.iter().position(|r| &**r == room) else {
            return false;
        };
        let key = entry.rooms.swap_remove(pos);
        leave_room(peers, rooms, &key, id, true);
        true
    }

    /// The IP group of a connected client.
    pub(crate) fn group_of(&self, id: Uuid) -> Option<IpGroup> {
        self.read().peers.get(&id).map(|p| p.group)
    }
}

/// Removes `id` from `room`'s member list, notifies the remaining members
/// (if `notify`) and drops the room once it is empty.
fn leave_room(peers: &HashMap<Uuid, PeerEntry>, rooms: &mut HashMap<Arc<str>, Vec<Uuid>>, room: &Arc<str>, id: Uuid, notify: bool) {
    let Some(members) = rooms.get_mut(room) else {
        return;
    };
    members.retain(|m| *m != id);
    if members.is_empty() {
        rooms.remove(room);
        return;
    }
    if !notify {
        return;
    }
    let left = encode(&ServerMessage::RoomPeerLeft { room, peer_id: id });
    for m in members.iter() {
        if let Some(p) = peers.get(m) {
            p.deliver(&left);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    struct TestPeer {
        id: Uuid,
        rx: mpsc::Receiver<Utf8Bytes>,
        ctl: Arc<ConnCtl>,
    }

    impl TestPeer {
        fn types(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(text) = self.rx.try_recv() {
                let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                out.push(v["type"].as_str().unwrap().to_owned());
            }
            out
        }
    }

    fn add(hub: &Hub, group: u8, ext: bool, nearby: bool, queue: usize) -> TestPeer {
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(queue);
        let ctl = Arc::new(ConnCtl::new());
        let info = Arc::new(ClientInfo {
            id,
            alias: "a".into(),
            version: "2.1".into(),
            device_model: None,
            device_type: None,
            token: "t".into(),
            ext: None,
        });
        hub.register(
            NewPeer { id, info, group: IpGroup::V4(Ipv4Addr::new(192, 0, 2, group)), nearby, ext, tx, ctl: Arc::clone(&ctl) },
            None,
        );
        TestPeer { id, rx, ctl }
    }

    #[test]
    fn group_membership_and_routes() {
        let hub = Hub::default();
        let mut a = add(&hub, 1, false, true, 8);
        let mut b = add(&hub, 1, true, true, 8);
        let mut c = add(&hub, 2, true, true, 8);
        let mut hidden = add(&hub, 1, true, false, 8);
        assert_eq!(a.types(), ["HELLO", "JOIN"]);
        assert_eq!(b.types(), ["HELLO"]);
        assert_eq!(c.types(), ["HELLO"]);
        assert_eq!(hidden.types(), ["HELLO"]);

        assert!(hub.route(a.id, b.id, false).is_ok());
        assert!(matches!(hub.route(b.id, a.id, true), Err(RouteError::Unsupported)));
        assert!(matches!(hub.route(a.id, c.id, false), Err(RouteError::NotFound)));
        assert!(matches!(hub.route(a.id, hidden.id, false), Err(RouteError::NotFound)));
        assert!(matches!(hub.route(a.id, a.id, false), Err(RouteError::NotFound)));

        let limits = Limits::default();
        assert!(hub.room_join(b.id, "c:000001", &limits).is_ok());
        assert!(hub.room_join(c.id, "c:000001", &limits).is_ok());
        assert!(hub.room_join(hidden.id, "c:000001", &limits).is_ok());
        assert!(hub.route(b.id, c.id, true).is_ok());
        assert!(hub.route(c.id, hidden.id, true).is_ok());
        assert_eq!(b.types(), ["ROOM_HELLO", "ROOM_PEER_JOINED", "ROOM_PEER_JOINED"]);
        assert_eq!(c.types(), ["ROOM_HELLO", "ROOM_PEER_JOINED"]);
        assert_eq!(hidden.types(), ["ROOM_HELLO"]);

        assert!(hub.room_leave(c.id, "c:000001"));
        assert!(!hub.room_leave(c.id, "c:000001"));
        assert!(matches!(hub.route(b.id, c.id, true), Err(RouteError::NotFound)));
        assert_eq!(b.types(), ["ROOM_PEER_LEFT"]);

        hub.unregister(b.id, true);
        assert_eq!(a.types(), ["LEFT"]);
        // One for `c` leaving the room, one for `b` disconnecting.
        assert_eq!(hidden.types(), ["ROOM_PEER_LEFT", "ROOM_PEER_LEFT"]);
        hub.unregister(hidden.id, true);
        assert!(hub.read().rooms.is_empty());
        assert!(a.types().is_empty());
    }

    #[test]
    fn quiet_unregister_sends_nothing() {
        let hub = Hub::default();
        let limits = Limits::default();
        let mut a = add(&hub, 1, true, true, 8);
        let b = add(&hub, 1, true, true, 8);
        assert!(hub.room_join(a.id, "c:000001", &limits).is_ok());
        assert!(hub.room_join(b.id, "c:000001", &limits).is_ok());
        a.types();
        assert_eq!(hub.unregister(b.id, false), 1);
        assert!(a.types().is_empty());
        assert_eq!(hub.read().rooms["c:000001"].len(), 1);
        assert_eq!(hub.read().groups[&IpGroup::V4(Ipv4Addr::new(192, 0, 2, 1))], [a.id]);
    }

    #[test]
    fn full_queue_kills_only_the_slow_peer() {
        let hub = Hub::default();
        let slow = add(&hub, 1, false, true, 1); // HELLO fills the queue
        let mut fast = add(&hub, 1, false, true, 8);
        assert_eq!(*slow.ctl.subscribe().borrow(), close::SLOW_CONSUMER);
        assert_eq!(*fast.ctl.subscribe().borrow(), 0);
        assert_eq!(fast.types(), ["HELLO"]);
    }

    #[test]
    fn room_limits() {
        let hub = Hub::default();
        let limits = Limits { max_room_members: 2, max_rooms_per_conn: 1, ..Limits::default() };
        let a = add(&hub, 1, true, true, 16);
        let b = add(&hub, 2, true, true, 16);
        let c = add(&hub, 3, true, true, 16);
        assert!(hub.room_join(a.id, "c:000001", &limits).is_ok());
        assert!(hub.room_join(a.id, "c:000001", &limits).is_ok());
        assert!(matches!(hub.room_join(a.id, "c:000002", &limits), Err(JoinError::TooManyRooms)));
        assert!(hub.room_join(b.id, "c:000001", &limits).is_ok());
        assert!(matches!(hub.room_join(c.id, "c:000001", &limits), Err(JoinError::RoomFull)));
        assert_eq!(hub.read().rooms["c:000001"].len(), 2);
    }
}
