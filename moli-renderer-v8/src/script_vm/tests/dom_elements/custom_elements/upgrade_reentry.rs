use super::*;

#[test]
fn pending_upgrade_reenters_through_append_child_and_explicit_upgrade() {
    let mut vm = new_storage_test_vm("https://custom-element-upgrade-reentry.test/");
    let result = vm.eval(r#"
      (() => {
        const results = [];
        for (const operation of ['append', 'upgrade']) {
          const frame = document.createElement('iframe');
          (document.body || document.documentElement || document).appendChild(frame);
          const w = frame.contentWindow, d = w.document;
          d.body.innerHTML = '<reentry-element id="a"></reentry-element><reentry-element id="b"></reentry-element>';
          const log = [];
          class Reentry extends w.HTMLElement {
            constructor() {
              super(); log.push(this.id + ':begin');
              const b = d.getElementById('b');
              if (operation === 'append') { b.remove(); d.body.appendChild(b); }
              else w.customElements.upgrade(b);
              log.push(this.id + ':end');
            }
          }
          w.customElements.define('reentry-element', Reentry);
          results.push(log);
          frame.remove();
        }
        return JSON.stringify(results);
      })()
    "#).expect("pending upgrades should participate in nested reaction scopes");
    assert_eq!(
        result,
        r#"[["a:begin","b:begin","b:end","a:end"],["a:begin","b:begin","b:end","a:end"]]"#
    );
}

#[test]
fn pending_duplicate_upgrade_preserves_initial_form_association_reaction() {
    let mut vm = new_storage_test_vm("https://custom-element-upgrade-form-reentry.test/");
    let result = vm
        .eval(
            r#"
      (() => {
        const form = document.createElement('form');
        form.id = 'owner';
        form.innerHTML = '<reentry-face id="a"></reentry-face><reentry-face id="b"></reentry-face>';
        (document.body || document.documentElement || document).appendChild(form);
        const log = [];
        class Reentry extends HTMLElement {
          static formAssociated = true;
          constructor() {
            super(); this.attachInternals(); log.push(this.id + ':begin');
            if (this.id === 'a') {
              const b = document.getElementById('b'); b.remove(); form.appendChild(b);
            }
            log.push(this.id + ':end');
          }
          connectedCallback() { log.push(this.id + ':connected'); }
          formAssociatedCallback(form) { log.push(this.id + ':form:' + form.id); }
        }
        customElements.define('reentry-face', Reentry);
        return JSON.stringify(log);
      })()
    "#,
        )
        .expect(
            "a remaining upgrade reaction must not suppress a completed upgrade's form callback",
        );
    assert_eq!(
        result,
        r#"["a:begin","b:begin","b:end","b:connected","b:form:owner","a:end","a:connected","a:form:owner"]"#
    );
}

#[test]
fn pending_duplicate_upgrade_clears_reactions_after_constructor_failure() {
    let mut vm = new_storage_test_vm("https://custom-element-upgrade-failure-reentry.test/");
    let result = vm.eval(r#"
      (() => {
        const parent = document.createElement('div');
        parent.innerHTML = '<reentry-failure id="a"></reentry-failure><reentry-failure id="b"></reentry-failure>';
        (document.body || document.documentElement || document).appendChild(parent);
        const constructors = [], connections = [];
        let errors = 0;
        addEventListener('error', event => { ++errors; event.preventDefault(); });
        class Reentry extends HTMLElement {
          constructor() {
            super(); constructors.push(this.id);
            if (this.id === 'b') throw new Error('failed upgrade');
            const b = document.getElementById('b'); b.remove(); parent.appendChild(b);
          }
          connectedCallback() { connections.push(this.id); }
        }
        customElements.define('reentry-failure', Reentry);
        const b = document.getElementById('b');
        customElements.upgrade(b);
        b.remove(); parent.appendChild(b);
        return JSON.stringify({constructors, connections, errors, defined:b.matches(':defined')});
      })()
    "#).expect("failed upgrades should discard duplicate upgrades and initial callbacks");
    assert_eq!(
        result,
        r#"{"constructors":["a","b"],"connections":["a"],"errors":1,"defined":false}"#
    );
}
