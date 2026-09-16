use super::*;

#[test]
fn child_parser_write_constructs_elements_and_runs_reactions_synchronously() {
    let mut vm = new_storage_test_vm("https://child-parser-custom-elements.test/");
    let result = vm.eval(r#"
      (() => {
        const results = [];
        for (const method of ['write', 'writeln']) {
          for (const explicitOpen of [false, true]) {
            const frame = document.createElement('iframe');
            (document.body || document.documentElement || document).appendChild(frame);
            const w = frame.contentWindow, d = w.document;
            if (explicitOpen) d.open();
            const log = [];
            class Written extends w.HTMLElement {
              constructor() { super(); log.push('construct:' + (this.ownerDocument === d)); }
              static get observedAttributes() { return ['title']; }
              attributeChangedCallback(name, oldValue, value) { log.push(name + ':' + oldValue + ':' + value); }
              connectedCallback() { log.push('connected'); }
            }
            class WrittenButton extends w.HTMLButtonElement {
              constructor() { super(); log.push('button'); }
              connectedCallback() { log.push('button-connected'); }
            }
            w.customElements.define('child-written', Written);
            w.customElements.define('child-button', WrittenButton, {extends:'button'});
            d[method]('<!doctype html><body><child-written title="parsed"></child-written><button is="child-button"></button>');
            results.push({
              custom:d.querySelector('child-written') instanceof Written,
              button:d.querySelector('button') instanceof WrittenButton,
              log:log.slice()
            });
            d.close();
            frame.remove();
          }
        }
        return JSON.stringify(results);
      })()
    "#).expect("child parser write and writeln should construct custom elements");
    let expected = serde_json::json!({
        "custom": true,
        "button": true,
        "log": ["construct:true", "title:null:parsed", "button", "connected", "button-connected"]
    });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).unwrap(),
        serde_json::Value::Array(vec![expected; 4])
    );
}

#[test]
fn child_parser_write_preserves_outer_javascript_microtask_boundary() {
    let mut vm = new_storage_test_vm("https://child-parser-microtasks.test/");
    let result = vm
        .eval(
            r#"
      globalThis.childParserLog = [];
      const frame = document.createElement('iframe');
      (document.body || document.documentElement || document).appendChild(frame);
      const w = frame.contentWindow, d = w.document;
      class Written extends w.HTMLElement {
        constructor() {
          super(); childParserLog.push('constructor');
          Promise.resolve().then(() => childParserLog.push('constructor-microtask'));
        }
        connectedCallback() { childParserLog.push('connected'); }
      }
      w.customElements.define('child-written', Written);
      Promise.resolve().then(() => childParserLog.push('earlier-microtask'));
      d.write('<!doctype html><body><child-written></child-written>');
      childParserLog.push('after-write');
      d.close();
      JSON.stringify(childParserLog);
    "#,
        )
        .expect("child document.write should keep outer JavaScript on the stack");
    assert_eq!(result, r#"["constructor","connected","after-write"]"#);
    assert_eq!(
        vm.eval("JSON.stringify(childParserLog)").unwrap(),
        r#"["constructor","connected","after-write","earlier-microtask","constructor-microtask"]"#
    );
}

#[test]
fn child_parser_write_queues_mutation_records_for_parser_insertions() {
    let mut vm = new_storage_test_vm("https://child-parser-observers.test/");
    let result = vm
        .eval(
            r#"
      (() => {
        const frame = document.createElement('iframe');
        (document.body || document.documentElement || document).appendChild(frame);
        const w = frame.contentWindow, d = w.document;
        d.open(); d.write('<!doctype html><body>');
        const observer = new w.MutationObserver(() => {});
        observer.observe(d.body, {childList:true, subtree:true});
        d.write('<b><i>hello</i></b>');
        const result = observer.takeRecords().map(r => [
          r.type, r.target.nodeName,
          Array.from(r.addedNodes, n => n.nodeName), r.removedNodes.length
        ]);
        observer.disconnect(); d.close(); frame.remove();
        return JSON.stringify(result);
      })()
    "#,
        )
        .expect("child parser mutations should be observable before write returns");
    assert_eq!(
        result,
        r##"[["childList","BODY",["B"],0],["childList","B",["I"],0],["childList","I",["#text"],0]]"##
    );
}

#[test]
fn child_parser_guards_dynamic_markup_during_construction_and_attribute_reactions() {
    let mut vm = new_storage_test_vm("https://child-parser-dynamic-markup.test/");
    let result = vm.eval(r#"
      (() => {
        const frame = document.createElement('iframe');
        const other = document.createElement('iframe');
        const root = document.body || document.documentElement || document;
        root.appendChild(frame); root.appendChild(other);
        const w = frame.contentWindow, d = w.document;
        const log = [];
        const probe = phase => {
          for (const method of ['open', 'close', 'write', 'writeln']) {
            try { d[method](''); log.push(phase + ':' + method + ':allowed'); }
            catch (e) { log.push(phase + ':' + method + ':' + e.name + ':' + (e instanceof w.DOMException)); }
          }
          other.contentDocument.write('<p>' + phase + '</p>');
        };
        class Written extends w.HTMLElement {
          constructor() { super(); probe('construct'); }
          static get observedAttributes() { return ['title']; }
          attributeChangedCallback() { probe('attribute'); }
        }
        w.customElements.define('child-written', Written);
        d.write('<!doctype html><body><child-written title="parsed"></child-written>');
        d.write('<p>after</p>');
        d.close(); other.contentDocument.close();
        return JSON.stringify({log, after:d.querySelector('p').textContent,
          other:Array.from(other.contentDocument.querySelectorAll('p'), p => p.textContent)});
      })()
    "#).expect("dynamic markup guards should cover construction and token attribute reactions");
    let mut log = Vec::new();
    for phase in ["construct", "attribute"] {
        for method in ["open", "close", "write", "writeln"] {
            log.push(format!("{phase}:{method}:InvalidStateError:true"));
        }
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).unwrap(),
        serde_json::json!({"log": log, "after":"after", "other":["construct", "attribute"]})
    );
}

#[tokio::test]
async fn child_parser_srcdoc_checkpoints_mutations_before_custom_element_construction() {
    for (markup, before) in [
        (
            "<b><child-parsed></child-parsed></b>",
            serde_json::json!([["BODY", "B"]]),
        ),
        (
            "<b><i>hello</b><child-parsed></child-parsed>",
            serde_json::json!([["BODY", "B"], ["B", "I"], ["I", "#text"], ["BODY", "I"]]),
        ),
    ] {
        let loader = ResourceRequestClient::new(&moli_fetch::FetchConfig::default()).unwrap();
        let mut vm = new_page_task_executor_test_vm_with_loader(
            "https://child-parser-checkpoint.test/",
            &loader,
        );
        let html = format!(
            r#"<!doctype html><body><script>
          window.batches = [];
          const describe = records => records.map(r => [r.target.nodeName,
            Array.from(r.addedNodes, n => n.nodeName).join(',')]);
          class Parsed extends HTMLElement {{
            constructor() {{ super(); window.beforeConstructor = batches.slice(); }}
          }}
          customElements.define('child-parsed', Parsed);
          new MutationObserver(records => batches.push(describe(records)))
            .observe(document.body, {{childList:true, subtree:true}});
        </script>{markup}</body>"#
        );
        vm.eval(&format!(
            r#"
          globalThis.childParserLoaded = false;
          globalThis.childParserFrame = document.createElement('iframe');
          childParserFrame.onload = () => {{ childParserLoaded = true; }};
          childParserFrame.srcdoc = {html:?};
          (document.body || document.documentElement || document).appendChild(childParserFrame);
          'queued';
        "#
        ))
        .expect("srcdoc parser probe should queue");
        advance_page_task_executor_until_eval_equals(
            &mut vm,
            &loader,
            "String(childParserLoaded)",
            "true",
            "child parser load",
        )
        .await;
        let result = vm
            .eval(
                r#"JSON.stringify({before:childParserFrame.contentWindow.beforeConstructor,
          after:childParserFrame.contentWindow.batches})"#,
            )
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result["before"],
            serde_json::json!([before.clone()]),
            "markup: {markup}"
        );
        let parent = if markup.contains("<i>") { "I" } else { "B" };
        assert_eq!(
            result["after"],
            serde_json::json!([before, [[parent, "CHILD-PARSED"]]]),
            "markup: {markup}"
        );
    }
}
