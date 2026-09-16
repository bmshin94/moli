use std::collections::HashSet;

use crate::{document_runtime::DomHandle, dom::native::DomHost};

/// Inherited style dependencies include the assigned slot; selector dependencies
/// also include the original DOM ancestry, even for nodes outside the flat tree.
/// Keep ShadowRoot handles on this path so tree-scoped invalidation roots cover
/// their styles. This is an invalidation path, not an inheritance parent.
pub(super) fn style_invalidation_parent(host: &DomHost, node: DomHandle) -> Option<DomHandle> {
    host.assigned_slot_for_node(node)
        .or_else(|| host.parent_node(node))
        .or_else(|| host.shadow_root_host(node))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StyleInvalidationCleanupEffects {
    clear_shadow_cascade_data_for_cleanup_target: bool,
}

impl StyleInvalidationCleanupEffects {
    pub(super) fn clear_shadow_cascade_data_for_cleanup_target() -> Self {
        Self {
            clear_shadow_cascade_data_for_cleanup_target: true,
        }
    }

    pub(super) fn clears_shadow_cascade_data_for_cleanup_target(self) -> bool {
        self.clear_shadow_cascade_data_for_cleanup_target
    }

    pub(super) fn extend(&mut self, other: Self) {
        self.clear_shadow_cascade_data_for_cleanup_target |=
            other.clear_shadow_cascade_data_for_cleanup_target;
    }
}

pub(super) fn handle_is_in_style_subtrees(
    host: &DomHost,
    handle: DomHandle,
    roots: &HashSet<DomHandle>,
) -> bool {
    let mut current = Some(handle);
    let mut seen = HashSet::new();
    while let Some(candidate) = current.filter(|candidate| seen.insert(*candidate)) {
        if roots.contains(&candidate) {
            return true;
        }
        current = style_invalidation_parent(host, candidate);
    }
    false
}
