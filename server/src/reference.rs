use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::Mutex;
use volcano_sfu::session::room::Room;

#[derive(Default)]
pub struct ReferenceDb {
    pub(crate) rooms: Arc<Mutex<BTreeMap<String, Arc<Room>>>>,
}
