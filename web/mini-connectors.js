import { action, button, card, field, form, node, row } from './mini-ui.js';
import { api, content, refresh, show, telegram } from './mini.js';

export function connectors(state) {
    const finder = card('Connect an MCP', 'Search by name, enter a public HTTPS endpoint, or enter a configured stdio:// name.');
    const matches = node('div');
    finder.append(form('Find connector', [field('name', 'Name or URL', {max: 2048})], async data => {
        const result = await api('search-mcp', data);
        matches.replaceChildren();
        if (!result.candidates.length) matches.append(node('p', 'No ready-to-connect endpoint found. Check the sources or enter an official endpoint.'));
        for (const candidate of result.candidates) {
            const entry = card(candidate.name, candidate.endpoint);
            entry.append(node('p', [candidate.publisher, candidate.transport].filter(Boolean).join(' · '), 'meta'));
            entry.append(action('Use this endpoint', 'add-mcp', {...candidate, confirmed: true}, 'Connector added. Select an authorization method.'));
            matches.append(entry);
        }
        for (const source of result.sources) {
            const entry = card(source.title, source.url);
            entry.append(button('Open source', () => telegram.openLink(source.url, {try_instant_view: false}), true));
            matches.append(entry);
        }
        for (const pkg of result.local_packages) {
            matches.append(card(pkg.name, [pkg.registry, pkg.identifier, 'Requires an operator-configured stdio program'].filter(Boolean).join(' · ')));
        }
        if (!result.web_search_available && !/^(https:|stdio:)/i.test(data.name)) matches.append(node('p', 'Web search returned no results. Showing registry results.', 'meta'));
    }), matches);
    content.append(finder, node('h2', 'Connectors'));
    for (const connector of state.connectors) {
        const item = card(connector.name, connector.endpoint);
        item.append(node('p', connector.auth === 'pending' ? 'Authorization needed' : 'Connected using ' + connector.auth));
        const consent = node('div');
        const openConsent = async data => {
            const result = await api('mcp-oauth', {...data, id: connector.id});
            const url = new URL(result.consent_url);
            if (url.protocol !== 'https:') throw new Error('The provider did not return an HTTPS consent page.');
            consent.replaceChildren(button('Open provider consent', () => telegram.openLink(url.href, {try_instant_view: false})));
        };
        if (connector.transport !== 'stdio') {
            const advanced = node('details');
            advanced.append(node('summary', 'Use an existing OAuth client'), form('Start OAuth with client', [
                field('client', 'OAuth client ID', {max: 2048}),
                field('secret', 'OAuth client secret (if required)', {type: 'password', max: 8192, required: false})
            ], async (data, element) => { try { await openConsent(data); } finally { element.reset(); } }));
            item.append(row(button('Start OAuth', () => openConsent({}))), advanced, consent);
        }
        item.append(row(action('Connect without a token', 'mcp-public', {id: connector.id}, 'Connector verified.', true),
            action('Remove connector', 'disconnect-mcp', {id: connector.id}, 'Connector removed.', true)));
        const token = node('details');
        const inputs = [field('token', 'API token', {type: 'password', max: 8192})];
        if (connector.transport !== 'stdio') inputs.push(
            field('header', 'Token header', {value: 'Authorization', max: 80, hint: 'Use the header listed by the provider, such as X-API-Key.'}),
            field('prefix', 'Header prefix', {value: 'Bearer', required: false, max: 32, hint: 'Leave blank when the header expects the raw key.'})
        );
        token.append(node('summary', 'Use an API token'),
            form('Connect with token', inputs, async (data, element) => {
                try {
                    await api('mcp-token', {...data, id: connector.id});
                    await refresh();
                    show('Connector verified.');
                } finally { element.reset(); }
            }));
        item.append(token);
        content.append(item);
    }
}
