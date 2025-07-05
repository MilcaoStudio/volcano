use std::{sync::Arc};
use volcano_sfu::rtc::{config::RouterConfig, room::{Room, RoomInfo}};

use crate::reference::ReferenceDb;

impl ReferenceDb {

    /// Get or create a Room by its ID
    pub async fn fetch_room(&self, id: &str) -> Option<Arc<Room>> {
        let rooms = self.rooms.lock().await;
        rooms.get(id).map(|room| room.clone())
    }
    
    pub async fn fetch_or_create_room(&self, id: &str, router_config: &RouterConfig) -> Arc<Room> {
        if let Some(room) = self.fetch_room(id).await {
            room
        } else {
            let mut rooms = self.rooms.lock().await;
            let room: Arc<Room> = Room::new(id.to_owned(), router_config);
            rooms.insert(id.to_string(), room.clone());
            room
        }
    }
    
    pub async fn fetch_available_rooms(&self, ids: &Vec<String>) -> Vec<RoomInfo> {
        let mut available_rooms = Vec::new();
        for id in ids {
            if let Some(room) = self.fetch_room(id).await {
                available_rooms.push(room.get_room_info())
            }
        }
        available_rooms
    }
}