import { action, button, card, field, form, node, row } from './mini-ui.js';
import { api, content, refresh, show, telegram } from './mini.js';

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
        const current = card('Connected model', state.provider + ' · ' + state.model);
        current.append(action('Disconnect model', 'disconnect-model', {}, 'Model disconnected.', true));
        content.append(current);
    }
    const router = card('OpenRouter', 'Use a tool-capable model from your OpenRouter account.');
    router.append(form('Connect OpenRouter', [field('model', 'Model ID', {max: 150}), field('key', 'API key', {type: 'password', max: 8192})],
        async (data, element) => {
            try {
                await api('openrouter', data);
                await refresh();
                show('OpenRouter connected.');
            } finally { element.elements.key.value = ''; }
        }));
    const codex = card('ChatGPT for Codex', 'Connect from a computer with the TinyBot CLI and a browser.');
    const pairing = node('pre');
    pairing.hidden = true;
    codex.append(form('Create pairing code', [field('model', 'Codex model ID', {max: 150})], async data => {
        const result = await api('codex-pair', data);
        pairing.textContent = result.command + '\n\nEnter this code when prompted\n' + result.code;
        pairing.hidden = false;
    }), pairing);
    const generator = card('Mercury 3', 'Connect OpenRouter to generate forms and short acknowledgements when TinyBot starts working.');
    generator.append(form('Save OpenRouter key', [field('key', 'OpenRouter API key', {type: 'password', max: 8192})], async (data, element) => {
        try {
            await api('openrouter-forms', data);
            await refresh();
            show('Mercury key saved.');
        } finally { element.reset(); }
    }));
    if (state.forms_key && state.provider !== 'openrouter') generator.append(row(action('Remove Mercury key',
        'disconnect-forms', {}, 'Mercury key removed.', true)));
    content.append(router, codex, generator);
}
