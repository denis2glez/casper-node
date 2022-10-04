use std::{collections::HashMap, time::Duration};

use crate::{
    components::fetcher::{metrics::Metrics, Event, FetchResponder, Fetcher, ItemFetcher},
    effect::{requests::StorageRequest, EffectBuilder, EffectExt, Effects},
    types::{BlockHash, ExecutedBlock, NodeId},
};

impl ItemFetcher<ExecutedBlock> for Fetcher<ExecutedBlock> {
    const SAFE_TO_RESPOND_TO_ALL: bool = false;

    fn responders(
        &mut self,
    ) -> &mut HashMap<BlockHash, HashMap<NodeId, Vec<FetchResponder<ExecutedBlock>>>> {
        &mut self.responders
    }

    fn validation_metadata(&self) -> &() {
        &()
    }

    fn metrics(&mut self) -> &Metrics {
        &self.metrics
    }

    fn peer_timeout(&self) -> Duration {
        self.get_from_peer_timeout
    }

    fn get_from_storage<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        id: BlockHash,
        peer: NodeId,
        _validation_metadata: (),
        responder: FetchResponder<ExecutedBlock>,
    ) -> Effects<Event<ExecutedBlock>>
    where
        REv: From<StorageRequest> + Send,
    {
        effect_builder
            .get_executed_block_from_storage(id)
            .event(move |result| Event::GetFromStorageResult {
                id,
                peer,
                validation_metadata: (),
                maybe_item: Box::new(result),
                responder,
            })
    }

    fn put_to_storage<REv>(
        &self,
        item: ExecutedBlock,
        peer: NodeId,
        effect_builder: EffectBuilder<REv>,
    ) -> Option<Effects<Event<ExecutedBlock>>>
    where
        REv: From<StorageRequest> + Send,
    {
        let item = Box::new(item);
        Some(
            effect_builder
                .put_executed_block_to_storage(item.clone())
                .event(move |_| Event::PutToStorage { item, peer }),
        )
    }
}
