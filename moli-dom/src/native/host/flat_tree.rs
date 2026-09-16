//! The flattened tree shared by CSS inheritance and box construction.
//! DOM parents remain authoritative for selectors, events and HTML semantics.

use super::{DomHandle, DomHost};

impl DomHost {
    /// Parent after shadow-tree substitution and slot assignment. Shadow roots
    /// are not flat-tree nodes. Unassigned light children and hidden fallback
    /// children have no flat-tree parent (their descendants keep local edges).
    pub fn flat_tree_parent(&self, node: DomHandle) -> Option<DomHandle> {
        let parent = self.parent_node(node)?;
        if self.shadow_root_handle(parent).is_some() {
            return self
                .node(node)
                .filter(|node| node.is_element() || node.is_text())
                .and_then(|_| self.assigned_slot_for_node(node));
        }
        if self.is_html_element_named(parent, "slot")
            && !self
                .assigned_nodes_for_slot_with_options(parent, false)
                .is_empty()
        {
            return None;
        }
        if self.is_shadow_root(parent) {
            return self.shadow_root_host(parent);
        }
        Some(parent)
    }

    /// Direct flat-tree children, including slots themselves. Slot assignment
    /// is deliberately not recursively flattened: slots still carry inherited
    /// styles even when their `display: contents` produces no layout box.
    pub fn flat_tree_children(&self, node: DomHandle) -> Vec<DomHandle> {
        if self.is_shadow_root(node) {
            return Vec::new();
        }
        let children = if let Some(shadow) = self.shadow_root_handle(node) {
            self.child_handles(shadow).collect()
        } else if self.is_html_element_named(node, "slot") {
            let assigned = self.assigned_nodes_for_slot_with_options(node, false);
            if assigned.is_empty() {
                self.child_handles(node).collect()
            } else {
                assigned
            }
        } else {
            self.child_handles(node).collect()
        };
        children
            .into_iter()
            .filter(|&child| self.flat_tree_parent(child) == Some(node))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{NativeDom, ShadowRootInit};

    #[test]
    fn flat_tree_keeps_nested_slots_and_tracks_manual_assignment() {
        let mut host = DomHost::from_dom(NativeDom::new_html(
            url::Url::parse("https://example.test/").unwrap(),
        ));
        let owner = host.create_element("div");
        let mut init = ShadowRootInit::new("open");
        init.set_slot_assignment("manual");
        let shadow = host.attach_shadow_root_with_init(owner, init).unwrap();
        let first = host.create_element("slot");
        let second = host.create_element("slot");
        let fallback = host.create_element("span");
        let nested_owner = host.create_element("div");
        let nested_root = host.attach_shadow_root(nested_owner, "open").unwrap();
        let nested_slot = host.create_element("slot");
        let text = host.create_text_node("nested text");
        assert!(host.append_child(shadow, first));
        assert!(host.append_child(shadow, second));
        assert!(host.append_child(first, fallback));
        assert!(host.append_child(owner, nested_owner));
        assert!(host.append_child(nested_root, nested_slot));
        assert!(host.append_child(nested_owner, text));

        assert_eq!(host.flat_tree_parent(nested_owner), None);
        assert_eq!(host.flat_tree_children(first), vec![fallback]);
        assert_eq!(
            host.assign_nodes_to_slot(first, vec![nested_owner]).len(),
            1
        );
        assert_eq!(host.flat_tree_children(first), vec![nested_owner]);
        assert_eq!(host.flat_tree_parent(nested_owner), Some(first));
        assert_eq!(host.flat_tree_parent(fallback), None);
        assert_eq!(host.flat_tree_children(nested_owner), vec![nested_slot]);
        assert_eq!(host.flat_tree_parent(nested_slot), Some(nested_owner));
        assert_eq!(host.flat_tree_children(nested_slot), vec![text]);
        assert_eq!(host.flat_tree_parent(text), Some(nested_slot));
        // Assignment changes the outer edge, not the nested inheritance chain.
        assert_eq!(
            host.assign_nodes_to_slot(second, vec![nested_owner]).len(),
            2
        );
        assert_eq!(host.flat_tree_parent(nested_owner), Some(second));
        assert_eq!(host.flat_tree_children(second), vec![nested_owner]);
        assert_eq!(host.flat_tree_children(first), vec![fallback]);
        assert_eq!(host.flat_tree_parent(fallback), Some(first));
        assert_eq!(host.flat_tree_parent(text), Some(nested_slot));
        assert_eq!(host.assign_nodes_to_slot(second, vec![]).len(), 1);
        assert_eq!(host.flat_tree_parent(nested_owner), None);
        assert!(host.flat_tree_children(second).is_empty());
        // The DOM relation does not follow either assignment.
        assert_eq!(host.parent_node(nested_owner), Some(owner));
        assert_eq!(host.parent_node(text), Some(nested_owner));
    }

    #[test]
    fn flat_parent_and_children_agree_across_assignments_and_fallback() {
        let mut host = DomHost::from_dom(NativeDom::new_html(
            url::Url::parse("https://example.test/").unwrap(),
        ));
        let owner = host.create_element("div");
        let shadow = host.attach_shadow_root(owner, "open").unwrap();
        let slot = host.create_element("slot");
        let fallback = host.create_element("span");
        let assigned = host.create_text_node("assigned");
        let missing = host.create_element("span");
        assert!(host.set_attribute(missing, "slot", "missing"));
        assert!(host.append_child(shadow, slot));
        assert!(host.append_child(slot, fallback));
        assert!(host.append_child(owner, missing));
        assert_eq!(host.flat_tree_parent(shadow), None);
        assert!(host.flat_tree_children(shadow).is_empty());
        assert_eq!(host.flat_tree_children(owner), vec![slot]);
        assert_eq!(host.flat_tree_parent(slot), Some(owner));
        assert_eq!(host.flat_tree_children(slot), vec![fallback]);
        assert_eq!(host.flat_tree_parent(fallback), Some(slot));
        assert_eq!(host.flat_tree_parent(missing), None);

        assert!(host.append_child(owner, assigned));
        assert_eq!(host.flat_tree_children(slot), vec![assigned]);
        assert_eq!(host.flat_tree_parent(assigned), Some(slot));
        assert_eq!(host.flat_tree_parent(fallback), None);
        assert!(host.remove_child(owner, assigned));
        assert_eq!(host.flat_tree_children(slot), vec![fallback]);
        assert_eq!(host.flat_tree_parent(fallback), Some(slot));
        // A slot outside a shadow tree is an ordinary DOM parent.
        assert!(host.append_child(owner, slot));
        assert_eq!(host.flat_tree_children(slot), vec![fallback]);
        assert_eq!(host.flat_tree_parent(fallback), Some(slot));
    }
}
