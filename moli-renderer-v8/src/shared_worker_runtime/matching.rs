use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use moli_shared_worker::{
    SharedWorkerClientId, SharedWorkerClientOwnerId, SharedWorkerClientRemoval,
    SharedWorkerConnectAction, SharedWorkerDescriptor, SharedWorkerInstanceId,
    SharedWorkerInstanceRemoval, SharedWorkerKey, SharedWorkerLoadFailure, SharedWorkerLoadReady,
    SharedWorkerRegistry, SharedWorkerRegistryDiagnostics,
};

use super::host::SharedRendererSharedWorkerHost;

#[derive(Clone, Debug)]
pub(crate) struct SharedWorkerClientOwnerIdAllocator {
    next: Arc<AtomicU64>,
}

impl Default for SharedWorkerClientOwnerIdAllocator {
    fn default() -> Self {
        Self {
            next: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl SharedWorkerClientOwnerIdAllocator {
    pub(crate) fn allocate(&self) -> SharedWorkerClientOwnerId {
        let id = self.next.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        SharedWorkerClientOwnerId::from_u64(id)
    }
}

#[derive(Default)]
pub(super) struct SharedWorkerMatchingStore {
    registry: SharedWorkerRegistry<SharedRendererSharedWorkerHost>,
    client_owner_id_allocator: SharedWorkerClientOwnerIdAllocator,
}

impl SharedWorkerMatchingStore {
    pub(super) fn with_client_owner_id_allocator(
        client_owner_id_allocator: SharedWorkerClientOwnerIdAllocator,
    ) -> Self {
        Self {
            client_owner_id_allocator,
            ..Self::default()
        }
    }

    pub(super) fn client_owner_id_allocator(&self) -> SharedWorkerClientOwnerIdAllocator {
        self.client_owner_id_allocator.clone()
    }

    pub(super) fn connect(
        &self,
        key: SharedWorkerKey,
        descriptor: SharedWorkerDescriptor,
        client_owner_id: SharedWorkerClientOwnerId,
    ) -> SharedWorkerConnectAction<SharedRendererSharedWorkerHost> {
        self.registry
            .connect_with_owner(key, descriptor, client_owner_id)
    }

    pub(super) fn finish_loading(
        &self,
        key: &SharedWorkerKey,
        instance_id: SharedWorkerInstanceId,
        host: SharedRendererSharedWorkerHost,
    ) -> SharedWorkerLoadReady<SharedRendererSharedWorkerHost> {
        self.registry.finish_loading(key, instance_id, host)
    }

    pub(super) fn fail_loading(
        &self,
        key: &SharedWorkerKey,
        instance_id: SharedWorkerInstanceId,
    ) -> SharedWorkerLoadFailure {
        self.registry.fail_loading(key, instance_id)
    }

    pub(super) fn remove_client(
        &self,
        client_id: SharedWorkerClientId,
    ) -> SharedWorkerClientRemoval<SharedRendererSharedWorkerHost> {
        self.registry.remove_client(client_id)
    }

    pub(super) fn remove_instance(
        &self,
        instance_id: SharedWorkerInstanceId,
    ) -> SharedWorkerInstanceRemoval<SharedRendererSharedWorkerHost> {
        self.registry.remove_instance(instance_id)
    }

    pub(super) fn remove_all_instances(
        &self,
    ) -> Vec<SharedWorkerInstanceRemoval<SharedRendererSharedWorkerHost>> {
        self.registry.remove_all_instances()
    }

    pub(super) fn running_host(
        &self,
        instance_id: SharedWorkerInstanceId,
    ) -> Option<SharedRendererSharedWorkerHost> {
        self.registry.running_instance(instance_id)
    }

    pub(super) fn diagnostics(&self) -> SharedWorkerRegistryDiagnostics {
        self.registry.diagnostics()
    }

    #[cfg(test)]
    pub(super) fn clients_for_instance(
        &self,
        instance_id: SharedWorkerInstanceId,
    ) -> Vec<SharedWorkerClientId> {
        self.registry.clients_for_instance(instance_id)
    }

    pub(super) fn loading_clients_for_instance(
        &self,
        instance_id: SharedWorkerInstanceId,
    ) -> Vec<SharedWorkerClientId> {
        self.registry.loading_clients_for_instance(instance_id)
    }

    #[cfg(test)]
    pub(super) fn next_client_owner_id(&self) -> SharedWorkerClientOwnerId {
        self.client_owner_id_allocator.allocate()
    }

    #[cfg(test)]
    pub(super) fn active_owner_ids_for_instance(
        &self,
        instance_id: SharedWorkerInstanceId,
    ) -> Vec<SharedWorkerClientOwnerId> {
        self.registry.client_owner_ids_for_instance(instance_id)
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }
}
