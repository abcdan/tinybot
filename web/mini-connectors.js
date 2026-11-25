import { action, button, card, field, form, node, row } from './mini-ui.js';
import { api, content, refresh, show, telegram } from './mini.js';

export function connectors(state) {
    const docblockAccess = state.docblock_access || {};
    if (!Object.keys(docblockAccess).length) content.append(docblockCard());
    content.append(anythingMcpSetup(state.anything_mcp_origin), findConnector(), node('h2', 'Connections'));
    for (const connector of state.connectors || []) {
        if (docblockAccess[connector.id]) {
            content.append(docblockCard(connector, docblockAccess[connector.id]));
            continue;
        }
        const gateway = connector.kind === 'anythingmcp';
        const connected = connector.auth !== 'pending';
        const item = card(connector.name, connector.endpoint);
        item.id = `connection-${connector.id}`;
        item.tabIndex = -1;
        const auth = {oauth: 'Signed in with OAuth', api: 'Connected with an API key', public: 'Connected without a token'};
        item.append(node('p', connected ? auth[connector.auth] || 'Connected' : 'Sign-in required', 'meta'));
        addApprovals(item, state, connector);
        if (gateway && connected) {
            item.append(toolBrowser(connector));
            const reconnect = node('details');
            reconnect.append(node('summary', 'Reconnect'), authorization(connector, true));
            item.append(reconnect);
        } else {
            item.append(authorization(connector, gateway));
        }
        if (['api', 'oauth'].includes(connector.auth)) item.append(calendarControls(connector));
        item.append(row(action('Remove connection', 'disconnect-mcp', {id: connector.id}, 'Connection removed.', true)));
        content.append(item);
    }
}

function calendarControls(connector) {
    const panel = node('div');
    const settings = node('div');
    const details = {id: connector.id};
    const load = async () => {
        const {calendar} = await api('calendar-settings', details);
        settings.replaceChildren(
            node('p', 'Select one calendar for availability checks and event reviews in Telegram. Requires a compatible Google Calendar MCP connection.'),
            form('Use this calendar', [
                field('account', 'Connected account alias', {max: 64, value: calendar?.account || ''}),
                field('calendar', 'Calendar ID', {max: 254, value: calendar?.calendar || 'primary'})
            ], async data => {
                await api('set-calendar', {...data, ...details});
                await load();
                show('Calendar selected.');
            })
        );
        if (calendar) settings.append(button('Clear calendar selection', async () => {
            await api('remove-calendar', details);
            await load();
            show('Calendar selection cleared.');
        }, true));
        settings.querySelector('input')?.focus();
    };
    panel.append(button('Calendar setup', load, true), settings);
    return panel;
}

function docblockCard(connector, access) {
    const status = {write: 'Read and write access', read: 'Read-only access', propose: 'Read and propose access', reauthorize: 'Sign in again to verify access'};
    const item = card('Docblock', status[access] || 'Save Markdown notes and share links from Telegram.');
    const consent = node('div');
    const label = access && access !== 'sign-in' ? 'Sign in again' : 'Connect Docblock';
    item.append(row(button(label, async () => {
        const {consent_url} = await api('docblock-oauth', connector ? {id: connector.id} : {});
        const url = new URL(consent_url);
        if (url.origin !== 'https://docblock.dev' || url.pathname !== '/oauth/authorize' || url.username || url.password) {
            throw new Error('Docblock returned an invalid sign-in link.');
        }
        consent.replaceChildren(row(
            button('Open Docblock sign-in', () => telegram.openLink(url.href, {try_instant_view: false})),
            button('Check connection', () => refresh(), true)
        ));
        consent.querySelector('button').focus();
    })), consent);
    if (access !== 'write') {
        item.append(node('p', 'To save or publish notes, choose Write notes directly during sign-in.'));
    }
    if (access === 'write') item.append(docblockShares(connector));
    if (connector) item.append(row(action('Remove connection', 'disconnect-mcp', {id: connector.id}, 'Connection removed.', true)));
    return item;
}

function docblockShares(connector) {
    const panel = node('div');
    const workspaceList = node('div');
    const shares = node('div');
    const loadShares = async workspace => {
        const details = {id: connector.id, workspace};
        const result = await api('docblock-shares', details);
        shares.replaceChildren();
        if (!result.shares.length) shares.append(node('p', 'No active public note links.'));
        for (const share of result.shares) {
            const item = card(share.target, share.expires_at ? `Expires ${new Date(share.expires_at).toLocaleDateString()}` : 'No expiry');
            const confirmation = node('div');
            item.append(button('Revoke link', () => confirmation.replaceChildren(
                node('p', 'Anyone using this link will lose access.'),
                row(button('Confirm revocation', async () => {
                    await api('revoke-docblock-share', {...details, share: share.id});
                    await loadShares(workspace);
                    show('Link revoked.');
                }), button('Keep link', () => confirmation.replaceChildren(), true))
            ), true), confirmation);
            shares.append(item);
        }
    };
    panel.append(button('Manage public note links', async () => {
        const result = await api('docblock-workspaces', {id: connector.id});
        const workspaces = result.workspaces.filter(workspace => ['owner', 'editor'].includes(workspace.role));
        workspaceList.replaceChildren();
        shares.replaceChildren();
        if (!workspaces.length) {
            workspaceList.append(node('p', 'No writable workspaces available.'));
            return;
        }
        workspaceList.append(form('View public note links', [field('workspace', 'Docblock workspace', {
            choices: workspaces.map(workspace => [workspace.id, workspace.name])
        })], ({workspace}) => loadShares(workspace)));
        await loadShares(workspaces[0].id);
    }, true), workspaceList, shares);
    return panel;
}

function anythingMcpSetup(origin) {
    const item = card('Connect AnythingMCP', origin ? '' : 'Copy the MCP server URL from your own AnythingMCP gateway.');
    if (origin) item.append(row(button('Open gateway', () => telegram.openLink(origin, {try_instant_view: false}), true)));
    const value = origin ? new URL('/mcp', origin).href : '';
    item.append(form('Continue', [field('endpoint', 'Gateway MCP URL', {type: 'url', max: 2048, value})], async ({endpoint}) => {
        const {id} = await api('add-anything-mcp', {endpoint: endpoint.trim()});
        await refresh();
        document.getElementById(`connection-${id}`)?.focus();
    }));
    return item;
}

function findConnector() {
    const finder = card('Connect an MCP', 'Search by name, enter a public HTTPS endpoint, or enter a configured stdio:// name.');
    const matches = node('div');
    finder.append(form('Find connector', [field('name', 'Name or URL', {max: 2048})], async data => {
        const result = await api('search-mcp', data);
        matches.replaceChildren();
        if (!result.candidates.length) matches.append(node('p', 'No ready-to-connect endpoint found. Check the sources or enter an official endpoint.'));
        for (const candidate of result.candidates) {
            const entry = card(candidate.name, candidate.endpoint);
            entry.append(node('p', [candidate.publisher, candidate.transport].filter(Boolean).join(' · '), 'meta'));
            entry.append(action('Use this endpoint', 'add-mcp', {...candidate, confirmed: true}, 'Connection added. Select an authorization method.'));
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
    return finder;
}

function addApprovals(item, state, connector) {
    const approvals = (state.mcp_approvals || []).filter(approval => approval.connector === connector.id);
    if (!approvals.length) return;
    item.append(node('h4', 'Always approved tools'));
    for (const {tool} of approvals) {
        const revoke = action('Revoke approval', 'revoke-mcp-approval', {connector: connector.id, tool}, 'Approval revoked.', true);
        revoke.setAttribute('aria-label', `Revoke approval for ${tool}`);
        item.append(row(node('p', tool), revoke));
    }
}

function authorization(connector, gateway) {
    const controls = node('div');
    if (connector.transport !== 'stdio') controls.append(oauthControls(connector, gateway));
    if (!gateway) controls.append(row(action('Connect without a token', 'mcp-public', {id: connector.id}, 'Connection verified.', true)));
    const token = node('details');
    const inputs = [field('token', gateway ? 'MCP API key' : 'API token', {type: 'password', max: 8192})];
    if (!gateway && connector.transport !== 'stdio') {
        inputs.push(
            field('header', 'Token header', {value: 'Authorization', max: 80, hint: 'Use the header listed by the provider, such as X-API-Key.'}),
            field('prefix', 'Header prefix', {value: 'Bearer', required: false, max: 32, hint: 'Leave blank when the header expects the raw key.'}),
        );
    }
    token.append(node('summary', gateway ? 'Use an MCP API key' : 'Use an API token'));
    if (gateway) token.append(node('p', 'Create a personal MCP API key on your gateway.'));
    token.append(form(gateway ? 'Connect with API key' : 'Connect with token', inputs, async (data, element) => {
        try {
            const credential = gateway ? {...data, header: 'X-API-Key', prefix: ''} : data;
            await api('mcp-token', {...credential, id: connector.id});
            await refresh();
            show('Connection verified.');
        } finally { element.reset(); }
    }));
    controls.append(token);
    return controls;
}

function oauthControls(connector, gateway) {
    const controls = node('div');
    const consent = node('div');
    const openConsent = async data => {
        const result = await api('mcp-oauth', {...data, id: connector.id});
        const url = new URL(result.consent_url);
        if (url.protocol !== 'https:' || url.username || url.password) throw new Error('The provider did not return a valid HTTPS consent page.');
        consent.replaceChildren(row(
            button(gateway ? 'Open gateway sign-in' : 'Open provider consent', () => telegram.openLink(url.href, {try_instant_view: false})),
            button('Check connection', () => refresh(), true),
        ));
        consent.querySelector('button').focus();
    };
    const advanced = node('details');
    advanced.append(node('summary', 'Use an existing OAuth client'), form('Start OAuth with client', [
        field('client', 'OAuth client ID', {max: 2048}),
        field('secret', 'OAuth client secret (if required)', {type: 'password', max: 8192, required: false})
    ], async (data, element) => { try { await openConsent(data); } finally { element.reset(); } }));
    controls.append(row(button(gateway ? 'Sign in to AnythingMCP' : 'Start OAuth', () => openConsent({}))), consent, advanced);
    return controls;
}

function toolBrowser(connector) {
    const panel = node('div');
    const tools = node('div');
    const load = button('View tools', async () => {
        const {tools: entries, total} = await api('mcp-tools', {id: connector.id});
        if (!entries.length) {
            tools.replaceChildren(node('p', 'This gateway exposes no tools for your account.'));
        } else {
            const heading = total > entries.length ? `Showing ${entries.length} of ${total} tools` : `Available tools (${total})`;
            const list = node('dl', undefined, 'connection-tools');
            for (const {name, description} of entries) {
                list.append(node('dt', name));
                if (description) list.append(node('dd', description));
            }
            tools.replaceChildren(node('h4', heading), list);
        }
        load.textContent = 'Refresh tools';
    }, true);
    panel.append(row(load), tools);
    return panel;
}
