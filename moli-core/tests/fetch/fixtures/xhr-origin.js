async function(cross) {
    const results = {};
    const home = location.origin;
    for (const opaque of [false, true]) {
        const kind = opaque ? 'opaque' : 'inherited';
        const frame = document.createElement('iframe');
        if (opaque) frame.setAttribute('sandbox', 'allow-scripts');
        const terminal = new Promise(resolve => {
            const handler = event => {
                if (event.source !== frame.contentWindow || event.data.kind !== kind) return;
                removeEventListener('message', handler);
                resolve(event.data.results);
            };
            addEventListener('message', handler);
        });
        frame.srcdoc = `<base href='${cross}/'><script>
            (async () => {
                const results = {};
                for (const async of [true, false]) {
                    for (const policy of ['denied', 'allowed', 'wrong', 'home']) {
                        const name = (async ? 'async' : 'sync') + '-' + policy;
                        const url = (policy === 'home' ? ${JSON.stringify(home)} : '')
                            + '/xhr-origin/${kind}/' + name;
                        results[name] = await new Promise(resolve => {
                            const xhr = new XMLHttpRequest();
                            xhr.open('GET', url, async);
                            xhr.onload = () => resolve('load:' + xhr.responseText);
                            xhr.onerror = () => resolve('error');
                            try {
                                xhr.send();
                                if (!async) resolve('load:' + xhr.responseText);
                            } catch (error) { resolve('error'); }
                        });
                    }
                }
                parent.postMessage({kind: '${kind}', results}, '*');
            })();
        <\/script>`;
        document.body.append(frame);
        results[kind] = await terminal;
        frame.remove();
    }
    return JSON.stringify(results);
}
