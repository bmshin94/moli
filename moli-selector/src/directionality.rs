//! HTML directionality is DOM semantics, independent of computed CSS direction
//! and flattened layout ancestry. This is the shared source for `:dir()` and
//! consumers such as form submission and presentation hints.

use crate::dom::{
    NodeId,
    native::{DomHost, Element, Node},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CssDirection {
    Ltr,
    Rtl,
}

impl CssDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ltr => "ltr",
            Self::Rtl => "rtl",
        }
    }
}

pub fn first_strong_text_direction(value: &str) -> Option<CssDirection> {
    value
        .chars()
        .find_map(|ch| match unicode_bidi::bidi_class(ch) {
            unicode_bidi::BidiClass::L => Some(CssDirection::Ltr),
            unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL => Some(CssDirection::Rtl),
            _ => None,
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HtmlDir {
    Undefined,
    Explicit(CssDirection),
    Auto,
}

impl HtmlDir {
    pub(crate) fn for_element(element: &Element) -> Self {
        if element.namespace() != "http://www.w3.org/1999/xhtml" {
            return Self::Undefined;
        }
        match element.attribute_ns("", "dir") {
            Some(value) if value.eq_ignore_ascii_case("ltr") => Self::Explicit(CssDirection::Ltr),
            Some(value) if value.eq_ignore_ascii_case("rtl") => Self::Explicit(CssDirection::Rtl),
            Some(value) if value.eq_ignore_ascii_case("auto") => Self::Auto,
            _ => Self::Undefined,
        }
    }
}

// A slot encountered during a contained-text scan contributes its shadow
// host's *direction*, not its fallback text. Carry that dependency back to
// the outer walk so nested shadow trees never require recursive resolution.
enum AutoDirectionality {
    Text(Option<CssDirection>),
    ShadowHost(NodeId),
}

/// Resolve HTML directionality without consulting CSS or the layout tree.
pub fn html_directionality(host: &DomHost, handle: NodeId) -> CssDirection {
    let mut current = Some(handle);
    while let Some(handle) = current {
        if let Some(element) = host.node(handle).and_then(Node::as_element) {
            let dir = HtmlDir::for_element(element);
            if let HtmlDir::Explicit(direction) = dir {
                return direction;
            }
            if dir == HtmlDir::Auto || element.is_html_element("bdi") {
                match auto_directionality(host, handle, element) {
                    AutoDirectionality::Text(direction) => {
                        return direction.unwrap_or(CssDirection::Ltr);
                    }
                    AutoDirectionality::ShadowHost(host) => {
                        current = Some(host);
                        continue;
                    }
                }
            }
            if element.is_html_input() && element.input_type() == "tel" {
                return CssDirection::Ltr;
            }
        }
        current = host
            .parent_node(handle)
            .or_else(|| host.shadow_root_host(handle));
    }
    CssDirection::Ltr
}

fn auto_directionality(host: &DomHost, root: NodeId, element: &Element) -> AutoDirectionality {
    if element.is_html_textarea()
        || (element.is_html_input() && input_uses_value_for_auto_direction(&element.input_type()))
    {
        let value = if element.is_html_textarea() && !element.input_value_dirty() {
            host.dom().direct_text_content(root).unwrap_or_default()
        } else {
            element.input_value()
        };
        return AutoDirectionality::Text(first_strong_text_direction(&value));
    }

    if element.is_html_element("slot") && host.containing_shadow_root(root).is_some() {
        let assigned = host.assigned_nodes_for_slot_with_options(root, false);
        if !assigned.is_empty() {
            for child in assigned {
                let Some(node) = host.node(child) else {
                    continue;
                };
                let direction = if let Some(text) = node.as_text() {
                    AutoDirectionality::Text(first_strong_text_direction(text.data()))
                } else if node
                    .as_element()
                    .is_some_and(|element| !excludes_auto_text(element))
                {
                    contained_text_directionality(host, child)
                } else {
                    continue;
                };
                if !matches!(direction, AutoDirectionality::Text(None)) {
                    return direction;
                }
            }
            return AutoDirectionality::Text(None);
        }
    }
    contained_text_directionality(host, root)
}

fn contained_text_directionality(host: &DomHost, root: NodeId) -> AutoDirectionality {
    let mut stack = host.child_handles_reversed(root).collect::<Vec<_>>();
    while let Some(handle) = stack.pop() {
        let Some(node) = host.node(handle) else {
            continue;
        };
        if let Some(text) = node.as_text() {
            if let Some(direction) = first_strong_text_direction(text.data()) {
                return AutoDirectionality::Text(Some(direction));
            }
            continue;
        }
        let Some(element) = node.as_element() else {
            continue;
        };
        if excludes_auto_text(element) {
            continue;
        }
        if element.is_html_element("slot")
            && let Some(shadow_root) = host.containing_shadow_root(handle)
            && let Some(shadow_host) = host.shadow_root_host(shadow_root)
        {
            return AutoDirectionality::ShadowHost(shadow_host);
        }
        stack.extend(host.child_handles_reversed(handle));
    }
    AutoDirectionality::Text(None)
}

fn excludes_auto_text(element: &Element) -> bool {
    ["bdi", "script", "style", "textarea"]
        .iter()
        .any(|name| element.is_html_element(name))
        || HtmlDir::for_element(element) != HtmlDir::Undefined
}

fn input_uses_value_for_auto_direction(input_type: &str) -> bool {
    matches!(
        input_type,
        "hidden"
            | "text"
            | "search"
            | "tel"
            | "url"
            | "email"
            | "password"
            | "submit"
            | "reset"
            | "button"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QueryEngine, dom::native::NativeDom};

    #[test]
    fn first_strong_direction_uses_unicode_bidi_classes() {
        for (text, expected) in [
            ("", None),
            ("123 .!?", None),
            ("\u{0661}\u{0662}\u{0663}", None),
            ("\u{05b0}\u{064b}", None),
            ("\u{0661}A", Some(CssDirection::Ltr)),
            ("\u{05b0}A", Some(CssDirection::Ltr)),
            ("\u{064b}\u{05d0}", Some(CssDirection::Rtl)),
            ("\u{200e}\u{05d0}", Some(CssDirection::Ltr)),
            ("\u{200f}A", Some(CssDirection::Rtl)),
            ("\u{061c}A", Some(CssDirection::Rtl)),
            ("\u{202b}A\u{202c}", Some(CssDirection::Ltr)),
            ("\u{1e900}A", Some(CssDirection::Rtl)),
            ("123 \u{05d0}A", Some(CssDirection::Rtl)),
            ("123 A\u{05d0}", Some(CssDirection::Ltr)),
        ] {
            assert_eq!(first_strong_text_direction(text), expected, "{text:?}");
        }
    }

    fn host() -> DomHost {
        let mut host = DomHost::from_dom(NativeDom::new_html(
            url::Url::parse("https://example.test/").unwrap(),
        ));
        host.reset_html_document_shell();
        host
    }

    fn assert_direction(host: &DomHost, node: NodeId, expected: CssDirection) {
        assert_eq!(html_directionality(host, node), expected);
        for direction in [CssDirection::Ltr, CssDirection::Rtl] {
            assert_eq!(
                QueryEngine
                    .matches_host(host, node, &format!(":dir({})", direction.as_str()))
                    .unwrap(),
                direction == expected,
            );
        }
    }

    #[test]
    fn auto_ignores_isolated_html_subtrees_but_not_foreign_namesakes() {
        let mut host = host();
        for tag in ["script", "style", "textarea", "bdi", "span"] {
            for namespace in ["http://www.w3.org/1999/xhtml", "http://www.w3.org/2000/svg"] {
                let root = host.create_element("div");
                assert!(host.set_attribute(root, "dir", "auto"));
                let excluded = host.create_element_ns(Some(namespace), tag).unwrap();
                if tag == "span" {
                    assert!(host.set_attribute(excluded, "dir", "ltr"));
                }
                assert!(host.set_text_content(excluded, "\u{05d0}"));
                assert!(host.append_child(root, excluded));
                let text = host.create_text_node("A");
                assert!(host.append_child(root, text));
                assert_direction(
                    &host,
                    root,
                    if namespace.ends_with("xhtml") {
                        CssDirection::Ltr
                    } else {
                        CssDirection::Rtl
                    },
                );
            }
        }
    }

    #[test]
    fn only_null_namespace_html_dir_attributes_control_direction() {
        let mut host = host();
        let parent = host.create_element("div");
        assert!(host.set_attribute(parent, "dir", "rtl"));
        let svg = host
            .create_element_ns(Some("http://www.w3.org/2000/svg"), "svg")
            .unwrap();
        assert!(host.set_attribute(svg, "dir", "ltr"));
        assert!(host.append_child(parent, svg));
        assert_direction(&host, svg, CssDirection::Rtl);
        let child = host.create_element("span");
        assert!(host.set_attribute_ns(child, Some("urn:test"), None, "dir", "ltr"));
        assert!(host.append_child(parent, child));
        assert_direction(&host, child, CssDirection::Rtl);
        for value in ["", "unknown", " ltr", "ltr "] {
            assert!(host.set_attribute_ns(child, None, None, "dir", value));
            assert_direction(&host, child, CssDirection::Rtl);
        }
        assert!(host.set_attribute_ns(child, None, None, "dir", "LtR"));
        assert_direction(&host, child, CssDirection::Ltr);
    }

    #[test]
    fn textarea_direction_uses_live_value_or_direct_default_text() {
        let mut host = host();
        let textarea = host.create_element("textarea");
        assert!(host.set_attribute(textarea, "dir", "auto"));
        assert!(host.set_text_content(textarea, "\u{05d0}"));
        assert_direction(&host, textarea, CssDirection::Rtl);
        assert!(host.set_input_value(textarea, "A"));
        assert_direction(&host, textarea, CssDirection::Ltr);
        assert!(host.set_input_value(textarea, "\u{1e900}"));
        assert_direction(&host, textarea, CssDirection::Rtl);
        assert!(host.set_input_value(textarea, "\u{0661}\u{05b0}"));
        assert_direction(&host, textarea, CssDirection::Ltr);
        assert!(host.set_input_value_with_dirty(textarea, "", false));
        assert_direction(&host, textarea, CssDirection::Rtl);
        assert!(host.set_text_content(textarea, ""));
        let descendant = host.create_element("span");
        assert!(host.set_text_content(descendant, "\u{05d0}"));
        assert!(host.append_child(textarea, descendant));
        assert_direction(&host, textarea, CssDirection::Ltr);
    }

    #[test]
    fn assigned_slot_auto_uses_assigned_nodes_then_fallback_content() {
        let mut host = host();
        let owner = host.create_element("div");
        let shadow = host.attach_shadow_root(owner, "open").unwrap();
        let slot = host.create_element("slot");
        assert!(host.set_attribute(slot, "dir", "auto"));
        assert!(host.set_text_content(slot, "\u{05d0}"));
        assert!(host.append_child(shadow, slot));
        assert_direction(&host, slot, CssDirection::Rtl);

        let assigned = host.create_element("span");
        assert!(host.set_text_content(assigned, "A"));
        assert!(host.append_child(owner, assigned));
        assert_direction(&host, slot, CssDirection::Ltr);
        assert!(host.set_text_content(assigned, "\u{05d0}"));
        assert_direction(&host, slot, CssDirection::Rtl);
        assert!(host.set_attribute(assigned, "dir", "rtl"));
        // A nonempty assignment list of excluded nodes does not expose fallback.
        assert_direction(&host, slot, CssDirection::Ltr);
        assert!(host.set_attribute(assigned, "slot", "other"));
        assert_direction(&host, slot, CssDirection::Rtl);
        let text = host.create_text_node("A");
        assert!(host.append_child(owner, text));
        assert_direction(&host, slot, CssDirection::Ltr);
    }

    #[test]
    fn contained_slot_contributes_host_direction_not_slot_fallback_text() {
        let mut host = host();
        let owner = host.create_element("div");
        assert!(host.set_attribute(owner, "dir", "rtl"));
        let shadow = host.attach_shadow_root(owner, "open").unwrap();
        let container = host.create_element("div");
        assert!(host.set_attribute(container, "dir", "auto"));
        assert!(host.append_child(shadow, container));
        let slot = host.create_element("slot");
        assert!(host.set_text_content(slot, "A"));
        assert!(host.append_child(container, slot));
        assert_direction(&host, container, CssDirection::Rtl);
        assert!(host.set_attribute(owner, "dir", "ltr"));
        assert!(host.set_text_content(slot, "\u{05d0}"));
        assert_direction(&host, container, CssDirection::Ltr);
        assert!(host.set_attribute(slot, "dir", "rtl"));
        let text = host.create_text_node("\u{05d0}");
        assert!(host.append_child(container, text));
        assert_direction(&host, container, CssDirection::Rtl);
    }

    #[test]
    fn direction_inherits_from_dom_parent_not_assigned_slot_or_css() {
        let mut host = host();
        let owner = host.create_element("div");
        assert!(host.set_attribute(owner, "dir", "ltr"));
        assert!(host.set_attribute(owner, "style", "direction:rtl"));
        let shadow = host.attach_shadow_root(owner, "open").unwrap();
        let slot = host.create_element("slot");
        assert!(host.set_attribute(slot, "dir", "rtl"));
        assert!(host.append_child(shadow, slot));
        let assigned = host.create_element("span");
        assert!(host.append_child(owner, assigned));
        assert_direction(&host, assigned, CssDirection::Ltr);
        let shadow_child = host.create_element("span");
        assert!(host.append_child(shadow, shadow_child));
        assert_direction(&host, shadow_child, CssDirection::Ltr);
        assert!(host.set_attribute(owner, "dir", "rtl"));
        assert_direction(&host, assigned, CssDirection::Rtl);
        assert_direction(&host, shadow_child, CssDirection::Rtl);
    }

    #[test]
    fn bdi_and_auto_form_controls_have_their_own_direction_source() {
        let mut host = host();
        let parent = host.create_element("div");
        assert!(host.set_attribute(parent, "dir", "rtl"));
        let bdi = host.create_element("bdi");
        assert!(host.append_child(parent, bdi));
        assert_direction(&host, bdi, CssDirection::Ltr);
        assert!(host.set_attribute(bdi, "dir", "invalid"));
        assert!(host.set_text_content(bdi, "\u{200f}A"));
        assert_direction(&host, bdi, CssDirection::Rtl);
        assert!(host.set_attribute(bdi, "dir", "ltr"));
        assert_direction(&host, bdi, CssDirection::Ltr);

        for kind in [
            "hidden", "text", "search", "tel", "url", "email", "password", "submit", "reset",
            "button",
        ] {
            let input = host.create_element("input");
            assert!(host.set_attribute(input, "type", kind));
            assert!(host.append_child(parent, input));
            assert_direction(
                &host,
                input,
                if kind == "tel" {
                    CssDirection::Ltr
                } else {
                    CssDirection::Rtl
                },
            );
            assert!(host.set_attribute(input, "dir", "auto"));
            assert!(host.set_input_value(input, "\u{200f}A"));
            assert_direction(&host, input, CssDirection::Rtl);
            assert!(host.set_input_value(input, "\u{0661}A"));
            assert_direction(&host, input, CssDirection::Ltr);
        }
        let number = host.create_element("input");
        assert!(host.set_attribute(number, "type", "number"));
        assert!(host.set_attribute(number, "dir", "auto"));
        assert!(host.append_child(parent, number));
        assert!(host.set_input_value(number, "123"));
        assert_direction(&host, number, CssDirection::Ltr);
    }

    #[test]
    fn nested_shadow_host_direction_dependencies_are_resolved_iteratively() {
        let mut host = host();
        let root = host.create_element("div");
        assert!(host.set_attribute(root, "dir", "rtl"));
        let mut owner = root;
        for _ in 0..512 {
            let shadow = host.attach_shadow_root(owner, "open").unwrap();
            let next = host.create_element("div");
            assert!(host.set_attribute(next, "dir", "auto"));
            assert!(host.append_child(shadow, next));
            let slot = host.create_element("slot");
            assert!(host.set_text_content(slot, "A"));
            assert!(host.append_child(next, slot));
            owner = next;
        }
        assert_direction(&host, owner, CssDirection::Rtl);
        assert!(host.set_attribute(root, "dir", "ltr"));
        assert_direction(&host, owner, CssDirection::Ltr);
    }
}
