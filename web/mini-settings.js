import { action, button, card, field, form, node, row } from './mini-ui.js';
import { api, content, refresh, show, telegram } from './mini.js';

export function browser({browser_proxy: proxy}) {
    const status = {
        saved: 'Using the saved proxy for browser traffic.',
        environment: 'Using the proxy configured in deployment settings.',
        direct: 'Using a direct browser connection.',
    }[proxy.source];
    if (!status) {
        content.append(card('Browser proxy', 'Proxy settings are unavailable. Tap Refresh to try again.'));
        return;
    }
    const settings = card('Browser proxy', status);
    const url = field('url', 'Proxy URL', {
        type: 'password', max: 2048,
        hint: 'Use a socks5:// or http:// URL. For a proxy with credentials, add username:password@ before the host.',
    });
    settings.append(form('Save proxy', [url], async (data, element) => {
        show('Saving browser proxy…');
        try {
            await api('save-browser-proxy', data);
            show('Browser proxy saved.');
            await refresh();
        } finally { element.reset(); }
    }));
    const controls = row();
    if (proxy.source !== 'direct') {
        controls.append(action('Use direct connection', 'disable-browser-proxy', {}, 'Browser direct connection enabled.', true));
    }
    if (proxy.overridden) {
        controls.append(action('Restore deployment setting', 'remove-browser-proxy', {}, 'Deployment setting restored.', true));
    }
    if (controls.childElementCount) settings.append(controls);
    content.append(settings);
}

export function schedules(state) {
    const zone = card('Time zone', 'Ask your Chief to set a reminder or schedule a task. For example, “Remind me to review the report every Monday at 9 am.”');
    zone.append(form('Save time zone', [field('zone', 'IANA time zone', {value: state.time_zone, max: 100})], async data => {
        await api('time-zone', data);
        await refresh();
        show('Time zone saved.');
    }));
    content.append(zone, node('h2', 'Reminders and scheduled tasks'));
    if (!state.schedules.length) content.append(card(null, 'No schedules yet.'));
    for (const schedule of state.schedules) {
        const next = schedule.next_run ? new Intl.DateTimeFormat(undefined, {
            dateStyle: 'medium', timeStyle: 'short', timeZone: schedule.time_zone
        }).format(new Date(schedule.next_run * 1000)) : '';
        const item = card(schedule.title, next ? next + ' · ' + schedule.time_zone : schedule.state);
        if (schedule.cron) item.append(node('p', schedule.cron, 'meta'));
        if (schedule.state === 'active') item.append(action('Cancel schedule', 'cancel-schedule', {id: schedule.id}, 'Schedule cancelled.', true));
        content.append(item);
    }
}

export function model(state) {
    if (state.provider) {
        const provider = state.provider === 'openrouter' ? 'OpenRouter' : 'ChatGPT for Codex';
        const current = card('Task model', provider + ' · ' + state.model);
        current.append(action('Disconnect task model', 'disconnect-model', {}, 'Task model disconnected.', true));
        content.append(current);
    }
    content.append(openRouter(state));
    if (state.openrouter_server_tools) content.append(openRouterTools(state.openrouter_server_tools));
    content.append(codexSettings(state));
}

function openRouterTools(settings) {
    const options = card('OpenRouter tools', 'Available when OpenRouter is the task model. TinyBot uses its local browser and search when a browser proxy is configured.');
    for (const [tool, name] of Object.entries({
        web_search: 'Web search', web_fetch: 'Web fetch', datetime: 'Date and time', search_models: 'Search models'
    })) {
        const label = node('label');
        const toggle = node('input');
        toggle.type = 'checkbox';
        toggle.checked = settings[tool] === true;
        label.append(toggle, document.createTextNode(name));
        options.append(label);
        toggle.addEventListener('change', async () => {
            toggle.disabled = true;
            try {
                await api('openrouter-server-tool', {tool, enabled: toggle.checked});
                settings[tool] = toggle.checked;
                show(`${name} ${toggle.checked ? 'enabled' : 'disabled'}.`);
            } catch (error) {
                toggle.checked = settings[tool] === true;
                show(error.message, true);
            } finally { toggle.disabled = false; }
        });
    }
    return options;
}

let prefetchedModels;

function openRouter(state) {
    const router = card('OpenRouter', 'Use one API key for OpenRouter task models and Mercury 3 forms and acknowledgements.');
    const key = form('Save API key', [field('key', 'OpenRouter API key', {type: 'password', max: 8192})], async (data, element) => {
        show('Checking your OpenRouter key…');
        try {
            const {models} = await api('openrouter-key', data);
            prefetchedModels = models;
            show('OpenRouter key saved.');
            await refresh();
        } finally { element.reset(); }
    });
    if (!state.openrouter_connected) {
        router.append(key);
        return router;
    }
    const catalog = node('div');
    const replace = node('details');
    replace.append(node('summary', 'Replace API key'), key);
    router.append(catalog, replace, button('Remove OpenRouter key', async () => {
        show('Removing the OpenRouter key…');
        await api('disconnect-openrouter');
        prefetchedModels = undefined;
        show('OpenRouter key removed.');
        await refresh();
    }, true));
    void loadOpenRouterModels(catalog, state);
    return router;
}

async function loadOpenRouterModels(catalog, state) {
    const status = node('p', 'Loading OpenRouter models…', 'meta');
    status.setAttribute('role', 'status');
    catalog.replaceChildren(status);
    const saved = prefetchedModels;
    prefetchedModels = undefined;
    try {
        const models = saved ?? (await api('openrouter-models')).models;
        if (!Array.isArray(models)) throw new Error('TinyBot could not load the OpenRouter model list.');
        if (!models.length) throw new Error('OpenRouter returned no tool-capable models.');
        const choices = [['', 'Choose a model'], ...models.map(({id, name}) => [
            id, name && name !== id ? name + ' (' + id + ')' : id
        ])];
        const current = state.provider === 'openrouter' ? state.model : '';
        catalog.replaceChildren(form('Use OpenRouter for tasks', [field('model', 'OpenRouter task model', {
            choices, value: models.some(({id}) => id === current) ? current : ''
        })], async data => {
            show('Saving your task model…');
            await api('openrouter-model', data);
            prefetchedModels = models;
            show('OpenRouter task model saved.');
            await refresh();
        }));
    } catch (error) {
        status.textContent = error.message;
        catalog.append(button('Retry model list', () => loadOpenRouterModels(catalog, state), true));
    }
}

function codexSettings(state) {
    const codex = card('ChatGPT for Codex', state.codex_model ? 'Connected · ' + state.codex_model
        : 'Connect from a computer with the TinyBot CLI and a browser.');
    const pairing = node('pre');
    pairing.hidden = true;
    const pair = form('Create pairing code', [field('model', 'Codex model ID', {max: 150, value: state.codex_model ?? ''})], async data => {
        show('Creating a pairing code…');
        const result = await api('codex-pair', data);
        show('');
        pairing.textContent = result.command + '\n\nEnter this code when prompted\n' + result.code;
        pairing.hidden = false;
    });
    if (!state.codex_model) {
        codex.append(pair, pairing);
        return codex;
    }
    const controls = row();
    if (state.provider !== 'codex') {
        controls.append(action('Use Codex for tasks', 'codex-select', {}, 'Codex task model saved.'));
    }
    controls.append(action('Disconnect Codex', 'disconnect-codex', {}, 'Codex disconnected.', true));
    const reconnect = node('details');
    reconnect.append(node('summary', 'Pair a different Codex model'), pair, pairing);
    codex.append(controls, reconnect);
    return codex;
}
