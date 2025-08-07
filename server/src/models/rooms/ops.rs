use std::{sync::Arc};
use volcano_sfu::rtc::{room::Room};

use crate::reference::ReferenceDb;

impl ReferenceDb {

    /// Get or create a Room by its ID
    pub async fn fetch_room(&self, id: &str) -> Option<Arc<Room>> {
        let rooms = self.rooms.lock().await;
        rooms.get(id).cloned()
    }
    
    pub async fn fetch_or_create_room(&self, id: &str) -> Arc<Room> {
        match self.fetch_room(id).await { Some(room) => {
            room
        } _ => {
            let mut rooms = self.rooms.lock().await;
            let room: Arc<Room> = Room::new(id.to_owned());
            rooms.insert(id.to_string(), room.clone());
            room
        }}
    }
}