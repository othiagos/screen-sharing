use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use shared_protocol::{PeerId, RoomId, ServerSignal};
use tokio::sync::mpsc;

pub type PeerSender = mpsc::UnboundedSender<ServerSignal>;

#[derive(Debug, Clone, Default)]
pub struct SignalingState {
    /// Maps `RoomId` -> Map of `PeerId` -> outbound channel sender.
    rooms: Arc<DashMap<RoomId, HashMap<PeerId, PeerSender>>>,
    /// Maps `PeerId` -> currently joined `RoomId` for fast cleanup on disconnect.
    peer_rooms: Arc<DashMap<PeerId, RoomId>>,
}

impl SignalingState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_room_count(&self) -> usize {
        self.rooms.len()
    }

    pub fn room_exists(&self, room_id: &str) -> bool {
        self.rooms.contains_key(room_id)
    }

    /// Creates a new room (or joins/initializes the given `room_id`) for `peer_id`.
    pub fn create_room(&self, peer_id: PeerId, tx: PeerSender) -> RoomId {
        self.leave_current_room(peer_id);

        let room_id = UuidShort::generate();
        let mut peers = HashMap::new();
        peers.insert(peer_id, tx.clone());
        self.rooms.insert(room_id.clone(), peers);
        self.peer_rooms.insert(peer_id, room_id.clone());

        let _ = tx.send(ServerSignal::RoomCreated {
            room_id: room_id.clone(),
            local_peer_id: peer_id,
        });

        room_id
    }

    /// Joins an existing room (or auto-creates it if not yet present so a user-chosen Room ID works seamlessly)
    /// and notifies existing members of the newcomer.
    pub fn join_room(&self, room_id: RoomId, peer_id: PeerId, tx: PeerSender) {
        self.leave_current_room(peer_id);

        let mut room_entry = self.rooms.entry(room_id.clone()).or_default();
        let existing_peers: Vec<PeerId> = room_entry.keys().copied().collect();

        for (&existing_id, existing_tx) in room_entry.iter() {
            if existing_id != peer_id {
                let _ = existing_tx.send(ServerSignal::PeerJoined { peer_id });
            }
        }

        room_entry.insert(peer_id, tx.clone());
        drop(room_entry);

        self.peer_rooms.insert(peer_id, room_id.clone());

        let _ = tx.send(ServerSignal::RoomJoined {
            room_id,
            local_peer_id: peer_id,
            existing_peers,
        });
    }

    /// Removes `peer_id` from its active room, broadcasts `PeerLeft`, and purges empty rooms.
    pub fn leave_current_room(&self, peer_id: PeerId) {
        if let Some((_, room_id)) = self.peer_rooms.remove(&peer_id) {
            let mut should_remove_room = false;
            if let Some(mut room_entry) = self.rooms.get_mut(&room_id) {
                room_entry.remove(&peer_id);
                for (&other_id, other_tx) in room_entry.iter() {
                    if other_id != peer_id {
                        let _ = other_tx.send(ServerSignal::PeerLeft { peer_id });
                    }
                }
                if room_entry.is_empty() {
                    should_remove_room = true;
                }
            }
            if should_remove_room {
                self.rooms.remove_if(&room_id, |_, peers| peers.is_empty());
            }
        }
    }

    /// Routes a direct signaling message strictly to `target` within the sender's room.
    pub fn route_to_peer(
        &self,
        from: PeerId,
        target: PeerId,
        signal: ServerSignal,
    ) -> Result<(), String> {
        let room_id = self
            .peer_rooms
            .get(&from)
            .map(|r| r.value().clone())
            .ok_or_else(|| "Peer is not currently in a room".to_string())?;

        let room = self
            .rooms
            .get(&room_id)
            .ok_or_else(|| format!("Room '{room_id}' does not exist"))?;

        let target_tx = room
            .get(&target)
            .ok_or_else(|| format!("Target peer {target} not found in room '{room_id}'"))?;

        target_tx
            .send(signal)
            .map_err(|_| format!("Target peer {target} disconnected"))
    }
}

struct UuidShort;

impl UuidShort {
    fn generate() -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        id[..8].to_string()
    }
}
