use crate::{
    audio::graph::RvcInstanceId,
    error::{BackendError, Result},
    rvc::adapter::RvcNodeProcessor,
};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

#[derive(Default)]
pub struct RvcManager {
    instances: RwLock<HashMap<RvcInstanceId, Arc<RvcNodeProcessor>>>,
}
impl RvcManager {
    pub fn register(&self, id: RvcInstanceId, processor: Arc<RvcNodeProcessor>) {
        self.instances.write().insert(id, processor);
    }
    pub fn remove(&self, id: RvcInstanceId) {
        self.instances.write().remove(&id);
    }
    pub fn get(&self, id: RvcInstanceId) -> Result<Arc<RvcNodeProcessor>> {
        self.instances
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| BackendError::Rvc(format!("RVC instance {} not found", id)))
    }
    pub fn list(&self) -> Vec<RvcInstanceId> {
        self.instances.read().keys().copied().collect()
    }
}
