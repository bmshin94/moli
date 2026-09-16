use super::*;

#[test]
fn insertion_upgrade_enqueues_connected_callback_once() {
    let mut vm = new_storage_test_vm("https://custom-element-insertion.test/");
    let result = vm.eval(r#"
      (() => {
        const results = [];
        for (const parentTag of ['option', 'div']) {
          for (const initiallyCustom of [false, true]) {
            const frame = document.createElement('iframe');
            (document.body || document.documentElement || document).appendChild(frame);
            const w = frame.contentWindow, d = w.document;
            const log = [];
            class Inserted extends w.HTMLElement {
              constructor() { super(); log.push('constructed'); }
              connectedCallback() { log.push('connected'); }
              disconnectedCallback() { log.push('disconnected'); }
            }
            w.customElements.define('inserted-element', Inserted);
            const parent = d.createElement(parentTag);
            d.body.appendChild(parent);
            const element = (initiallyCustom ? d : document).createElement('inserted-element');
            const created = log.splice(0);
            parent.appendChild(element);
            const inserted = log.splice(0);
            if (parentTag === 'option') parent.text = 'replacement';
            else parent.textContent = 'replacement';
            const removed = log.splice(0);
            parent.appendChild(element);
            const reconnected = log.splice(0);
            results.push({created, inserted, removed, reconnected, custom:element instanceof Inserted});
            frame.remove();
          }
        }
        return JSON.stringify(results);
      })()
    "#).expect("insertion upgrades should own their initial connected callback");
    let results: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
    assert_eq!(results.len(), 4);
    for (index, result) in results.iter().enumerate() {
        let (created, inserted) = if index % 2 == 0 {
            (
                serde_json::json!([]),
                serde_json::json!(["constructed", "connected"]),
            )
        } else {
            (
                serde_json::json!(["constructed"]),
                serde_json::json!(["connected"]),
            )
        };
        assert_eq!(
            result,
            &serde_json::json!({
                "created":created, "inserted":inserted, "removed":["disconnected"],
                "reconnected":["connected"], "custom":true
            }),
            "variant: {index}"
        );
    }
}

#[test]
fn insertion_upgrade_preserves_reaction_order_with_already_custom_siblings() {
    let mut vm = new_storage_test_vm("https://custom-element-insertion-order.test/");
    let result = vm
        .eval(
            r#"
      (() => {
        const frame = document.createElement('iframe');
        (document.body || document.documentElement || document).appendChild(frame);
        const w = frame.contentWindow, d = w.document;
        const log = [];
        class Inserted extends w.HTMLElement {
          constructor() { super(); log.push('construct:' + this.id); }
          connectedCallback() { log.push('connected:' + this.id); }
        }
        w.customElements.define('inserted-element', Inserted);
        const parent = d.createElement('div');
        const a = d.createElement('inserted-element'); a.id = 'a';
        const b = document.createElement('inserted-element'); b.id = 'b';
        const c = d.createElement('inserted-element'); c.id = 'c';
        parent.append(a, b, c);
        log.length = 0;
        d.body.appendChild(parent);
        return JSON.stringify(log);
      })()
    "#,
        )
        .expect("insertion reactions should follow subtree order regardless of upgrade state");
    assert_eq!(
        result,
        r#"["connected:a","construct:b","connected:b","connected:c"]"#
    );
}
