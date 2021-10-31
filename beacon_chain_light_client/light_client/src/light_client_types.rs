use types::{BeaconBlockHeader, Fork, SyncCommittee, AggregateSignature, BitVector, EthSpec};
extern crate ethereum_types;

pub type Hash256 = ethereum_types::H256;

pub struct LightClientSnapshot<T: EthSpec> {
    pub header: BeaconBlockHeader,
    pub current_sync_committee: SyncCommittee<T>,
    pub next_sync_committee: SyncCommittee<T>
}

pub struct LightClientStore<T: EthSpec> {
    pub snaphost: LightClientSnapshot<T>,
    pub valid_updates: Vec<LightClientUpdate<T>>
}

pub struct LightClientUpdate<T: EthSpec> {
    pub header: BeaconBlockHeader,
    pub next_sync_committee: SyncCommittee<T>,
    // TO DO: change to FixedVector<Hash256, T::SIZE_OF_VECTOR>,
    pub next_sync_committee_branch: Vec<Hash256>,
    pub finality_header: BeaconBlockHeader,
    // TO DO: change to FixedVector<Hash256, T::SIZE_OF_VECTOR>,
    pub finality_branch: Vec<Hash256>,
    pub sync_committee_bits: BitVector<T::SyncCommitteeSize>,
    pub sync_committee_signature: AggregateSignature,
    // Can potentially be the value of `Fork.current_version`, which is a `[u8; 4]`
    pub fork: Fork
}

pub struct SyncPeriod(u64);

// Not sure if this impl is necessary, but it's what `Slot` and `Epoch` implement
impl SyncPeriod {
    pub const fn new(period: u64) -> SyncPeriod {
        SyncPeriod(period)
    }
}
