use types::{BeaconBlockHeader};
use crate::light_client_types::SyncPeriod;

pub struct LightClientEvents {
    pub new_header: BeaconBlockHeader,
    pub new_sync_period: SyncPeriod
}