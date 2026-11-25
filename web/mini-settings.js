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
    const editor = node('div');
    const templates = card('Create a routine');
    const choose = (draft = {}) => {
        editor.replaceChildren(routineForm(state, draft, false));
        editor.querySelector('input').focus();
    };
    templates.append(row(
        button('New routine', () => choose()),
        button('Inbox briefing', () => choose({
            title: 'Inbox briefing',
            instructions: 'Summarize recent messages in folders enabled for automatic handling.',
            source: 'inbox',
            steps: [
                {instruction: 'Identify messages that need attention. Omit codes, secrets and credentials.'},
                {instruction: 'Summarize what needs action and what is informational.'},
            ],
        }), true),
        button('Weekly research digest', () => choose({
            title: 'Weekly research digest',
            instructions: 'Track public developments in [topic].',
            cron: '0 9 * * 1',
            steps: [
                {instruction: 'Find public developments on the topic from the last seven days. Open sources to verify each item.'},
                {instruction: 'Write a short digest with source links, dates, and why each item matters.'},
            ],
        }), true),
        button('Meeting decisions', () => choose({
            title: 'Meeting decisions',
            instructions: 'Use the meeting notes I provide. Ask for them if they are missing.',
            steps: [
                {instruction: 'Extract decisions, owners, and due dates stated in the notes.'},
                {instruction: 'Draft a Markdown note with decisions and action items.'},
            ],
        }), true),
    ), editor);
    content.append(zone, templates, node('h2', 'Reminders, scheduled tasks, and routines'));
    if (!state.schedules.length) content.append(card(null, 'No schedules yet.'));
    for (const schedule of state.schedules) content.append(scheduleCard(state, schedule));
}

function scheduleCard(state, schedule) {
    const active = schedule.state === 'active';
    const editable = active || ['paused', 'manual'].includes(schedule.state);
    const label = {paused: 'Paused', manual: 'Manual', done: 'Completed', cancelled: 'Cancelled', invalid: 'Invalid schedule'};
    const next = active && schedule.next_run
        ? scheduleTime(schedule.next_run, schedule.time_zone) + ' · ' + schedule.time_zone
        : label[schedule.state];
    const item = card(schedule.title, next);
    const snoozed = schedule.snoozed_until > Date.now() / 1000;
    const delayedUntil = Math.max(schedule.held_until ?? 0, schedule.snoozed_until ?? 0);
    if (active && delayedUntil > Date.now() / 1000) {
        item.append(node('p', `${snoozed ? 'Snoozed' : 'Deferred'} until ${scheduleTime(delayedUntil, schedule.time_zone)}`, 'meta'));
    }
    const {id, revision} = schedule;
    const controls = row();
    if (editable || schedule.state === 'done') {
        const request_id = crypto.randomUUID();
        controls.append(action('Run now', 'run-schedule', {id, revision, request_id}, 'Run queued.'));
    }
    if (active || schedule.state === 'paused') {
        controls.append(active
            ? action('Pause', 'pause-schedule', {id, revision}, 'Schedule paused.', true)
            : action('Resume', 'resume-schedule', {id, revision}, 'Schedule resumed.', true));
    }
    if (active) {
        controls.append(snoozed
            ? action('End snooze', 'snooze-schedule', {id, revision, minutes: 0}, 'Snooze ended.', true)
            : action('Snooze for 1 hour', 'snooze-schedule', {id, revision, minutes: 60}, 'Snoozed for one hour.', true));
    }
    if (editable) {
        controls.append(action('Cancel schedule', 'cancel-schedule', {id}, 'Schedule cancelled.', true));
        item.append(controls, schedulePanel('Edit schedule', async () => {
            const draft = await api('schedule-details', {id});
            return routineForm(state, draft, true);
        }), schedulePanel('Timing policy', async () => schedulePolicy(schedule)));
        if (schedule.kind !== 'reminder') {
            item.append(schedulePanel('Dry run', async () => previewForm(schedule)));
            if (active || schedule.state === 'manual') item.append(schedulePanel('Webhook', () => routineWebhook(schedule)));
        }
    } else if (controls.childElementCount) item.append(controls);
    item.append(schedulePanel('Run history', async () => {
        const {runs, occurrences = []} = await api('schedule-history', {id});
        const history = node('div');
        if (!runs.length && !occurrences.length) history.append(node('p', 'No runs yet.'));
        const status = {idle: 'Finished', setup: 'Needs setup', completed: 'Finished', running: 'Working', waiting: 'Needs you'};
        const seen = new Set(occurrences.map(occurrence => occurrence.task).filter(Boolean));
        const receipts = node('div');
        history.append(receipts);
        const labels = {deferred: 'Deferred', skipped_overlap: 'Skipped · previous run still open',
            skipped_missed: 'Skipped · missed occurrence', coalesced: 'Combined into one run',
            invalid: 'Invalid schedule', superseded: 'Schedule changed'};
        const delivery = {sent: 'Delivered', queued: 'Delivery queued', sending: 'Sending', failed: 'Delivery failed', uncertain: 'Delivery unconfirmed'};
        let displayed = 0;
        const older = button('Show older occurrences', showMore, true);
        showMore();
        history.append(older);
        function showMore() {
            const page = occurrences.slice(displayed, displayed + 30);
            for (const occurrence of page) {
                const result = occurrence.notification_state === 'unchanged' ? 'Unchanged · no message sent'
                    : labels[occurrence.outcome] ?? delivery[occurrence.delivery]
                    ?? status[occurrence.task_state] ?? occurrence.task_state ?? 'Run queued';
                const entry = card(scheduleTime(occurrence.due_at, schedule.time_zone), result);
                if (occurrence.delayed_until) {
                    entry.append(node('p', `Held until ${scheduleTime(occurrence.delayed_until, schedule.time_zone)}`, 'meta'));
                }
                if (occurrence.task && occurrence.task_state !== 'cancelled') {
                    entry.append(action('Continue in chat', 'select-task', {task: occurrence.task}, 'Send your next message in Telegram.', true));
                }
                receipts.append(entry);
            }
            displayed += page.length;
            older.hidden = displayed >= occurrences.length;
        }
        for (const run of runs.filter(run => !seen.has(run.task))) {
            const type = {manual: 'Manual', schedule: 'Scheduled', webhook: 'Webhook'};
            const details = [
                scheduleTime(run.started_at ?? run.updated, schedule.time_zone),
                run.preview ? 'Dry run' : type[run.trigger],
                run.notification_state === 'unchanged' ? 'Unchanged · no message sent' : status[run.state] ?? run.state,
            ].filter(Boolean).join(' · ');
            const entry = card(run.title, details);
            if (!run.preview && run.state !== 'cancelled') entry.append(action('Continue in chat', 'select-task', {task: run.task}, 'Send your next message in Telegram.', true));
            history.append(entry);
        }
        return history;
    }));
    return item;
}

async function routineWebhook(schedule) {
    const {webhook} = await api('schedule-webhook', {id: schedule.id});
    const panel = node('div');
    if (webhook) {
        panel.append(node('p', webhook.enabled ? 'Webhook enabled.' : 'Routine changed. Enable a fresh webhook.'));
        if (webhook.enabled) panel.append(node('code', webhook.url));
        panel.append(row(action('Revoke webhook', 'revoke-schedule-webhook', {id:schedule.id, hook:webhook.id}, 'Webhook revoked.', true)));
    }
    panel.append(row(button(webhook?.enabled ? 'Rotate signing key' : 'Enable webhook', async () => {
        const hook = await api('enable-schedule-webhook', {id:schedule.id, revision:schedule.revision});
        panel.replaceChildren(
            node('p', 'Endpoint'), node('code', hook.url),
            node('p', 'Signing key'), node('code', hook.secret),
            node('p', 'Save the signing key now. It is shown once.', 'meta'),
            row(action('Revoke webhook', 'revoke-schedule-webhook', {id:schedule.id, hook:hook.id}, 'Webhook revoked.', true)),
        );
    })));
    return panel;
}

function previewForm(schedule) {
    const request_id = crypto.randomUUID();
    return form('Preview in Telegram', [field('sample', 'Sample input', {
        type: 'textarea', max: 2000, hint: 'Use sample data to see what the saved steps would produce.',
    })], async ({sample}) => {
        await api('preview-routine', {id: schedule.id, revision: schedule.revision, request_id, sample});
        await refresh();
        show('Dry run queued. The preview will arrive in Telegram.');
    });
}

function schedulePolicy(schedule) {
    const localTime = value => Number.isInteger(value)
        ? `${String(Math.floor(value / 60)).padStart(2, '0')}:${String(value % 60).padStart(2, '0')}` : '';
    const minutes = value => {
        if (!value) return null;
        const [hour, minute] = value.split(':').map(Number);
        return hour * 60 + minute;
    };
    const fields = [
        field('catch_up', 'After downtime', {choices: [['coalesce', 'Run once'], ['skip', 'Skip missed recurring runs']], value: schedule.catch_up ?? 'coalesce'}),
        field('quiet_start', 'Quiet hours start', {type: 'time', required: false, value: localTime(schedule.quiet_start), hint: schedule.time_zone}),
        field('quiet_end', 'Quiet hours end', {type: 'time', required: false, value: localTime(schedule.quiet_end)}),
    ];
    if (schedule.kind !== 'reminder') fields.push(field('notify_changes', 'Automation notifications', {
        choices: [['0', 'Every finished run'], ['1', 'Only when the result changes']],
        value: schedule.notify_changes ? '1' : '0',
    }));
    return form('Save timing policy', fields, async data => {
        await api('schedule-policy', {id: schedule.id, revision: schedule.revision,
            policy: {catch_up: data.catch_up, quiet_start: minutes(data.quiet_start), quiet_end: minutes(data.quiet_end),
                notify_changes: data.notify_changes === '1'}});
        await refresh();
        show('Timing policy saved.');
    });
}

function scheduleTime(time, timeZone) {
    return new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium', timeStyle: 'short', timeZone
    }).format(new Date(time * 1000));
}

function schedulePanel(label, load) {
    const panel = node('details');
    const target = node('div');
    panel.append(node('summary', label), target);
    let requested = false;
    panel.addEventListener('toggle', async () => {
        if (!panel.open || requested) return;
        requested = true;
        target.replaceChildren(node('p', 'Loading…', 'meta'));
        try { target.replaceChildren(await load()); }
        catch (error) {
            requested = false;
            target.replaceChildren(node('p', error.message));
        }
    });
    return panel;
}

function routineForm(state, draft, editing) {
    const timing = field('timing', 'When to run', {
        choices: [['manual', 'Manually'], ['once', 'Once'], ['recurring', 'On a schedule']],
        value: draft.cron ? 'recurring' : draft.local_at ? 'once' : 'manual',
    });
    const timezone = draft.time_zone ?? state.time_zone;
    const at = field('at', 'Date and time', {type: 'datetime-local', value: draft.local_at, hint: timezone});
    const cron = field('cron', 'Cron expression', {value: draft.cron ?? '', max: 110, hint: `Five fields in ${timezone}, such as 0 9 * * 1 for Mondays at 9 am.`});
    const scheduleControls = [at, cron];
    const timingControl = timing.querySelector('select');
    const updateTiming = () => scheduleControls.forEach((label, index) => {
        label.hidden = timingControl.value !== ['once', 'recurring'][index];
        label.querySelector('input').disabled = label.hidden;
    });
    timingControl.addEventListener('change', updateTiming);
    updateTiming();
    const steps = node('div');
    if (draft.kind !== 'reminder') routineSteps(steps, state, draft);
    return form(editing ? 'Save changes' : 'Save routine', [
        field('title', 'Title', {value: draft.title, max: 100}),
        field('input', draft.kind === 'reminder' ? 'Reminder text' : 'Instructions', {type: 'textarea', value: draft.instructions, max: 2000}),
        timing, ...scheduleControls, steps,
    ], async data => {
        const names = [...steps.querySelectorAll('fieldset')];
        const routine = {
            title: data.title, input: data.input,
            steps: names.map(entry => {
                const instruction = entry.querySelector('textarea').value;
                const agent = entry.querySelector('select')?.value;
                return {instruction, agent: agent || null};
            }),
        };
        if (data.at) routine.at = data.at === draft.local_at ? draft.scheduled_at : data.at;
        if (data.cron) routine.cron = data.cron;
        if (editing) await api('edit-schedule', {id: draft.id, revision: draft.revision, routine});
        else await api('create-routine', {source: draft.source, routine});
        await refresh();
        show(editing ? 'Schedule updated.' : 'Routine saved.');
    });
}

function routineSteps(steps, state, draft) {
    const owned = (state.agents ?? []).filter(agent => agent.owner === state.user);
    const list = node('div');
    const add = button('Add step', () => appendStep({}), true);
    const update = () => {
        add.hidden = list.children.length >= 6;
        [...list.children].forEach((entry, index) => {
            entry.querySelector('legend').textContent = `Step ${index + 1}`;
        });
    };
    const appendStep = step => {
        const entry = node('fieldset');
        const instruction = field('instruction', 'Step instructions', {type: 'textarea', max: 600, value: step.instruction});
        entry.append(node('legend'), instruction);
        if (draft.source !== 'inbox') {
            const choices = [['', 'Chief'], ...owned.map(agent => [agent.id, agent.name])];
            if (step.agent && !owned.some(agent => agent.id === step.agent)) choices.push([step.agent, 'Unavailable helper']);
            entry.append(field('agent', 'Assigned to', {choices, value: step.agent ?? '', required: false}));
        }
        entry.append(button('Remove step', () => { entry.remove(); update(); }, true));
        list.append(entry);
        update();
    };
    steps.append(list, add);
    for (const step of draft.steps ?? [{}]) appendStep(step);
}

export function model(state) {
    if (state.provider) {
        const provider = state.provider === 'openrouter' ? 'OpenRouter' : 'ChatGPT for Codex';
        const current = card('Task model', provider + ' · ' + state.model);
        current.append(action('Disconnect task model', 'disconnect-model', {}, 'Task model disconnected.', true));
        content.append(current);
        content.append(backupModel(state));
    }
    content.append(openRouter(state));
    if (state.image_settings) content.append(imageModel(state));
    if (state.openrouter_server_tools) content.append(openRouterTools(state.openrouter_server_tools));
    content.append(codexSettings(state));
}

function imageModel(state) {
    const item = card('Image model');
    if (!state.openrouter_connected) {
        item.append(node('p', 'Connect OpenRouter to generate and edit images.'));
        return item;
    }
    const {selected, choices} = state.image_settings;
    item.append(form('Save image model', [field('model', 'Image generation and editing model', {
        choices: choices.map(({id, name}) => [id, name]), value: selected,
    })], async data => {
        await api('save-image-model', data);
        await refresh();
        show('Image model saved.');
    }));
    return item;
}

function backupModel(state) {
    const backup = state.backup_model;
    const providers = {codex: 'ChatGPT for Codex', openrouter: 'OpenRouter'};
    const item = card('Backup model', backup ? `${providers[backup.provider]} · ${backup.model}` : 'Off');
    item.append(node('p', 'TinyBot can switch once if the task model is temporarily unavailable, before any tools run.'));
    if (backup) item.append(action('Turn off backup model', 'clear-backup-model', {}, 'Backup model turned off.', true));
    if (state.provider === 'openrouter' && state.codex_model && backup?.model !== state.codex_model) {
        item.append(action('Use Codex as backup', 'save-backup-model', {provider: 'codex', model: state.codex_model}, 'Backup model saved.'));
    } else if (state.provider === 'codex' && state.openrouter_connected) {
        const options = node('div');
        item.append(button('Choose OpenRouter backup', async () => {
            const {models} = await api('openrouter-models');
            const choices = [['', 'Choose a model'], ...models.map(({id, name}) => [id, name || id])];
            options.replaceChildren(form('Save backup model', [field('model', 'OpenRouter backup model', {
                choices, value: models.some(({id}) => id === backup?.model) ? backup.model : ''
            })], async data => {
                await api('save-backup-model', {...data, provider: 'openrouter'});
                await refresh();
                show('Backup model saved.');
            }));
        }, true), options);
    } else if (!backup) {
        item.append(node('p', 'Connect the other provider below to choose a backup.'));
    }
    return item;
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
    const router = card('OpenRouter', 'Use one API key for OpenRouter task models and Mercury 3 forms.');
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
