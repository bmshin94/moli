async function(cross) {
    const results = {};
    const home = location.origin;
    for (const opaque of [false, true]) {
        const kind = opaque ? 'opaque' : 'inherited';
        results[kind] = {};
        const cases = ['classic', 'module', 'style', 'image', 'eventsource', 'track']
            .flatMap(resource => ['denied', 'allowed'].map(policy => ({resource, policy})));
        cases.push({resource: 'track', policy: 'home'}, {resource: 'track', policy: 'cross'});
        for (const {resource, policy} of cases) {
            const name = resource + '-' + policy;
            const frame = document.createElement('iframe');
            if (opaque) frame.setAttribute('sandbox', 'allow-scripts');
            const terminal = new Promise(resolve => {
                const handler = event => {
                    if (event.source !== frame.contentWindow || event.data.name !== name) return;
                    removeEventListener('message', handler);
                    resolve(event.data.result);
                };
                addEventListener('message', handler);
            });
            const url = (policy === 'home' ? home : '') + '/subresource-origin/' + (name === 'style-allowed' ? 'shared' : kind) + '/' + name;
            const base = `<base href='${cross}/'>`;
            if (resource === 'classic' || resource === 'module') {
                frame.srcdoc = base + `<script type='${resource === 'module' ? 'module' : 'text/javascript'}'
                    crossorigin='anonymous' src='${url}'
                    onload="parent.postMessage({name:'${name}', result:'load'}, '*')"
                    onerror="parent.postMessage({name:'${name}', result:'error'}, '*')"><\/script>`;
            } else {
                frame.srcdoc = base + `<body><script>
                    const done = result => parent.postMessage({name: '${name}', result}, '*');
                    try {
                        const resource = '${resource}';
                        const url = ${JSON.stringify(url)};
                        if (resource === 'eventsource') {
                            const stream = new EventSource(url);
                            stream.onmessage = event => { stream.close(); done(event.data); };
                            stream.onerror = () => { stream.close(); done('error'); };
                        } else {
                            const element = document.createElement(
                                resource === 'style' ? 'link' : resource === 'image' ? 'img' : 'track');
                            element.onload = () => {
                                try {
                                    if (resource === 'style' && element.sheet.cssRules.length !== 1) throw new Error('missing CSS rules');
                                    done('load');
                                } catch (error) { done(String(error)); }
                            };
                            element.onerror = () => done('error');
                            if (resource === 'style') {
                                element.rel = 'stylesheet';
                                element.crossOrigin = 'anonymous';
                                element.href = url;
                            } else {
                                if (resource === 'image') element.crossOrigin = 'anonymous';
                                element.src = url;
                            }
                            if (resource === 'track') {
                                const media = document.createElement('video');
                                if ('${policy}' === 'denied' || '${policy}' === 'allowed') media.crossOrigin = 'anonymous';
                                media.append(element);
                                document.body.append(media);
                                element.track.mode = 'hidden';
                            } else document.head.append(element);
                        }
                    } catch (error) { done(String(error)); }
                <\/script>`;
            }
            document.body.append(frame);
            results[kind][name] = await terminal;
            frame.remove();
        }
    }
    return JSON.stringify(results);
}
