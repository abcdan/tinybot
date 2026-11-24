import { action, button, card, field, form, node, row } from './mini-ui.js';
import { agentForm, agents } from './mini-agents.js';
import { audit } from './mini-audit.js';
import { connectors } from './mini-connectors.js';
import { invites } from './mini-invites.js';
import { mail } from './mini-mail.js';
import { memories } from './mini-memory.js';
import { onboarding } from './mini-onboarding.js';
import { browser, schedules, model } from './mini-settings.js';

export const telegram = window.Telegram?.WebApp;
export const content = document.querySelector('#view');
const notice = document.querySelector('#status');
const refreshButton = document.querySelector('#refresh');
const chiefLabel = document.querySelector('#chief-label');
const shell = content.closest('main');
const tabs = document.querySelector('#tabs');
const browserTab = tabs.querySelector('[data-tab="browser"]');
const invitesTab = tabs.querySelector('[data-tab="invites"]');
const sourceLink = document.querySelector('#source-link');
const footer = sourceLink.closest('footer');
const params = new URLSearchParams(window.location.search);
let formId = params.get('form');
const openInTelegram = 'Open TinyBot from its Telegram profile or tap Settings & forms in the bot chat.';
let state;
let tab = params.get('connect') === 'docblock' ? 'connectors' : 'home';
let generation = 0;
let opened = false;
let disposeOnboarding;
let completedTasksOpen = false;

export function show(text, error = false) {
    notice.textContent = text;
    notice.hidden = !text;
    notice.classList.toggle('error', error);
}

export async function api(action, data = {}) {
    if (!telegram?.initData) throw new Error(openInTelegram);
    const response = await fetch('/api/mini', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({init: telegram.initData, action, data})
    });
    const result = await response.json();
    if (!response.ok) {
        throw Object.assign(new Error(result.error || 'The request did not complete.'), {status: response.status});
    }
    return result;
}

export async function refresh(animate = false) {
    const sequence = ++generation;
    try {
        const result = formId ? await api('get-form', {id: formId}) : await api('status');
        if (sequence !== generation) return;
        const needsOnboarding = !formId && result.onboarding_required === true;
        tabs.hidden = Boolean(formId) || needsOnboarding;
        refreshButton.hidden = needsOnboarding;
        footer.hidden = needsOnboarding;
        if (!opened) {
            opened = true;
            openFullscreen();
        }
        show('');
        if (needsOnboarding) {
            state = undefined;
            openOnboarding(result);
            return;
        }
        closeOnboarding();
        if (formId) {
            let item;
            if (result.kind === 'settings-form') {
                item = settingsForm(result);
            } else if (result.kind === 'settings-approve') {
                item = settingsReview(result);
            } else {
                item = card('Answer a few questions', 'Your answers go to your Chief.');
                item.append(generatedForm(result));
            }
            content.replaceChildren(item);
        } else {
            state = result;
            chiefLabel.textContent = state.chief_name || 'Your Chief';
            browserTab.hidden = state.browser_proxy == null;
            invitesTab.hidden = state.operator !== true;
            if ((tab === 'browser' && browserTab.hidden) || (tab === 'invites' && invitesTab.hidden)) setTab('home');
            else render(animate);
        }
    } catch (error) {
        if (sequence === generation) {
            closeOnboarding();
            state = undefined;
            tabs.hidden = true;
            footer.hidden = true;
            refreshButton.hidden = !telegram?.initData || error.status === 403;
            content.replaceChildren();
            show(error.status === 403 ? openInTelegram : error.message, true);
        }
    }
}

function openOnboarding(profile) {
    if (disposeOnboarding) return;
    shell.classList.add('onboarding');
    content.setAttribute('aria-label', 'TinyBot setup');
    disposeOnboarding = onboarding(content, profile, {
        api,
        finish: () => refresh(true),
        cancel: () => {
            closeOnboarding();
            const item = card('Set up TinyBot');
            item.append(button('Start setup', () => refresh()));
            content.replaceChildren(item);
            telegram?.close?.();
        },
        openLink: url => {
            if (!telegram?.openLink) return false;
            telegram.openLink(url);
            return true;
        },
    });
}

function closeOnboarding() {
    if (!disposeOnboarding) return;
    disposeOnboarding();
    disposeOnboarding = undefined;
    shell.classList.remove('onboarding');
    content.setAttribute('aria-label', 'Settings');
}

function openFullscreen() {
    const expand = () => {
        try { telegram.expand?.(); }
        catch { /* Settings still work at the current height. */ }
    };
    try {
        if (telegram.isFullscreen) return;
        if (telegram.isVersionAtLeast?.('8.0') && telegram.requestFullscreen) {
            telegram.onEvent?.('fullscreenFailed', ({error}) => {
                if (error === 'UNSUPPORTED') expand();
            });
            telegram.requestFullscreen();
            return;
        }
    } catch {
        // Some clients expose the method without fullscreen support.
    }
    expand();
}

function setTab(next) {
    const changed = tab !== next;
    tab = next;
    for (const item of document.querySelectorAll('#tabs button')) {
        if (item.dataset.tab === tab) item.setAttribute('aria-current', 'page');
        else item.removeAttribute('aria-current');
    }
    render(changed);
}

function render(animate = false) {
    if (!state) return;
    content.replaceChildren();
    ({home, agents, memories, connectors, mail, browser, schedules, audit, invites, model})[tab](state);
    if (animate && !window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
        content.getAnimations?.().forEach(animation => animation.cancel());
        content.animate?.([{opacity: 0, transform: 'translateY(6px)'}, {opacity: 1, transform: 'translateY(0)'}],
            {duration: 180, easing: 'ease-out'});
    }
}

function home() {
    if (!state.provider) {
        const setup = card('Connect a model', 'Add an OpenRouter key or connect an eligible ChatGPT plan to give your Chief a model account.');
        setup.append(button('Set up model', () => setTab('model')));
        content.append(setup);
    }
    const requestsHeading = node('h2', 'Requests');
    requestsHeading.id = 'task-requests';
    content.append(requestsHeading);
    if (!state.requests.length) content.append(card(null, 'You’re all caught up.'));
    for (const request of state.requests) {
        if (request.kind === 'settings-form') {
            content.append(settingsForm(request));
            continue;
        }
        if (request.kind === 'settings-approve') {
            content.append(settingsReview(request));
            continue;
        }
        const details = request.content;
        const title = { 'agent-spec': 'Configure a helper', form: 'Enter form details', 'form-submit': 'Submit website form', elicitation: 'Answer a few questions' }[request.kind];
        const item = card(details.operation || title, details.destination);
        if (request.kind === 'agent-spec') {
            item.append(agentForm(null, details, request.id));
            item.append(row(action('Decline', 'deny', {id: request.id}, 'Request declined.', true)));
        } else if (request.kind === 'form' || request.kind === 'elicitation') {
            if (request.kind === 'elicitation') item.append(node('p', 'Your answers go to your Chief.'));
            item.append(generatedForm(request), row(action('Decline', 'deny', {id: request.id}, 'Request declined.', true)));
        } else if (request.kind === 'email-send') {
            const data = {id: request.id};
            item.append(node('pre', details.explanation), row(
                action('Approve once', 'approve', data, 'Send approved.'),
                action('Deny', 'deny', data, 'Send denied.', true)
            ));
            if (details.draft) {
                const draft = details.draft;
                const edit = node('details');
                edit.append(node('summary', 'Edit email draft'), form('Save for review', [
                    field('to', 'Recipients, separated by commas', {value: draft.to.join(', '), max: 1024}),
                    field('subject', 'Subject', {value: draft.subject, max: 180}),
                    field('body', 'Message', {value: draft.body, type: 'textarea', max: 2400})
                ], async values => {
                    await api('edit-email-draft', {id: request.id, draft: {
                        to: values.to.split(',').map(address => address.trim()).filter(Boolean),
                        subject: values.subject, body: values.body
                    }});
                    await refresh();
                    show('Draft updated. Review it before sending.');
                }));
                item.append(edit);
            }
        } else if (details.docblock) {
            const preview = details.docblock;
            const data = {id: request.id};
            const deny = action('Deny', 'deny', data, 'Request denied.', true);
            item.append(node('p', `Workspace: ${preview.workspace}`), node('p', `Note: ${preview.target}`), node('pre', preview.change));
            if (preview.expires_in_days != null) {
                const expires = field('expires_in_days', 'Expires after (days)', {type: 'number', value: preview.expires_in_days});
                const days = expires.querySelector('input');
                days.min = '1';
                days.max = '365';
                item.append(form('Approve once', [expires], async values => {
                    await api('approve-docblock-share', {...data, expires_in_days: Number(values.expires_in_days)});
                    await refresh();
                    show('Publication approved.');
                }), deny);
            } else {
                item.append(row(action('Approve once', 'approve', data, 'Change approved.'), deny));
            }
        } else if (details.mcp) {
            const data = {id: request.id};
            const choices = [action('Approve once', 'approve', data, 'Approved for this call.')];
            if (details.rememberable) {
                choices.push(action('Always approve', 'always-approve-mcp', data, 'Approval saved for this tool.', true));
            }
            choices.push(action('Deny', 'deny', data, 'Request denied.', true));
            item.append(node('p', details.explanation), row(...choices));
        } else {
            if (details.preview || details.details) item.append(node('pre', details.preview || details.details));
            item.append(row(action('Approve', 'approve', {id: request.id}, 'Approved. TinyBot will continue.'),
                action('Decline', 'deny', {id: request.id}, 'Request declined.', true)));
        }
        content.append(item);
    }
    tasks();
}

function tasks() {
    content.append(node('h2', 'Tasks'));
    const results = node('div');
    let searchGeneration = 0;
    async function search(query, offset = 0) {
        const sequence = ++searchGeneration;
        const page = await api('search-tasks', {query, offset});
        if (sequence !== searchGeneration || !results.isConnected) return;
        if (!offset) results.replaceChildren();
        taskPage(results, page, next => search(query, next), !offset && 'No matching tasks.');
    }
    const finder = form('Search tasks', [field('query', 'Search retained messages and tasks', {
        max: 200, required: false, type: 'search'
    })], ({query}) => search(query.trim()));
    const open = state.tasks.filter(task => task.state !== 'completed');
    if (!open.length) results.append(card(null, state.tasks.length ? 'No open tasks.' :
        'Send a message to your Chief in Telegram to start a task.'));
    results.append(...open.map(taskCard));
    if (state.tasks.length) results.append(completedTaskHistory());
    content.append(finder, results);
}

function taskPage(results, page, load, empty) {
    if (!page.items.length && empty) results.append(card(null, empty));
    results.append(...page.items.map(taskCard));
    if (page.next_offset != null) {
        const more = button('Load more tasks', async () => {
            await load(page.next_offset);
            more.remove();
        }, true);
        results.append(more);
    }
}

function completedTaskHistory() {
    const history = node('details', undefined, 'task-history');
    const results = node('div');
    let loaded = false;
    history.append(node('summary', 'Completed tasks'), results);
    history.open = completedTasksOpen;
    async function load(offset = 0) {
        const page = await api('search-tasks', {query: '', offset, completed_only: true});
        if (!history.isConnected) return;
        if (!offset) results.replaceChildren();
        taskPage(results, page, load, !offset && 'No completed tasks yet.');
    }
    history.addEventListener('toggle', async () => {
        if (!history.isConnected) return;
        completedTasksOpen = history.open;
        if (!history.open || loaded) return;
        loaded = true;
        results.replaceChildren(card(null, 'Loading completed tasks…'));
        try {
            await load();
        } catch (error) {
            loaded = false;
            results.replaceChildren(card(null, error.message));
        }
    });
    return history;
}

function taskCard(task) {
    const labels = {
        idle: 'Ready for a follow-up', running: 'Working', waiting: 'Waiting for your response',
        setup: 'Waiting for model setup', cancelled: 'Stopped', blocked: 'Stopped with an error',
        uncertain: 'Check the destination before continuing', queued: 'Queued', completed: 'Completed'
    };
    const item = card(task.title, labels[task.state] || task.state);
    item.classList.add('task-card');
    const controls = [];
    if (task.state === 'completed') {
        controls.push(action('Reopen', 'select-task', {task: task.id}, 'Task reopened. Send your next message in Telegram.', true));
    } else if (task.id !== state.active_task && task.state !== 'cancelled') {
        controls.push(action('Continue', 'select-task', {task: task.id}, 'Send your next message in Telegram.', true));
    } else if (task.id === state.active_task) item.append(node('span', 'Current Chief task', 'meta'));
    if (['idle', 'blocked', 'queued', 'running', 'waiting', 'setup'].includes(task.state) && Number.isSafeInteger(task.revision)) {
        controls.push(action('Mark complete', 'complete-task', {task: task.id, revision: task.revision}, 'Task marked complete.', true));
    }
    if (['queued', 'running', 'waiting', 'setup'].includes(task.state)) {
        controls.push(action('Stop', 'stop-task', {task: task.id, revision: task.revision}, 'Task stopped.', true));
    }
    if (task.state === 'waiting') controls.push(button('Review requests', () => document.querySelector('#task-requests')?.scrollIntoView(), true));
    if (controls.length) item.append(row(...controls));
    return item;
}

function requestInputs(fields) {
    return fields.map(spec => {
        if (spec.type === 'checkbox') {
            const label = node('label');
            const control = node('input');
            control.type = 'checkbox';
            control.name = spec.name;
            control.value = 'true';
            control.required = spec.required;
            control.checked = spec.value === 'true' || spec.value === true;
            label.append(control, document.createTextNode(spec.label));
            if (spec.hint) label.append(node('span', spec.hint, 'field-hint'));
            return label;
        }
        const choices = spec.type === 'select' ? [['', 'Choose an option'], ...spec.options.map(value => [value, value])] : undefined;
        return field(spec.name, spec.label, {
            type: spec.private && spec.type === 'text' ? 'password' : spec.type,
            choices, required: spec.required, max: 4096, hint: spec.hint, value: spec.value ?? undefined
        });
    });
}

function generatedForm(request) {
    const website = request.kind === 'form';
    const label = website ? 'Continue to review' : 'Send answers';
    return form(label, requestInputs(request.content.fields), async (values, element) => {
        await api(website ? 'submit-form' : 'answer-form', {id: request.id, values});
        element.reset();
        if (formId) {
            finishFocusedForm('Answers sent', 'TinyBot will continue in your chat.');
            return;
        }
        await refresh();
        show(website ? 'Details received. Review the submission below.' : 'Answers sent. TinyBot will continue.');
    });
}

function settingsForm(request) {
    const item = card(request.content.operation || 'Configure TinyBot');
    item.append(form('Review changes', requestInputs(request.content.fields), async values => {
        const review = await api('answer-settings-form', {id: request.id, values});
        if (formId) {
            generation++;
            formId = review.id;
            const url = new URL(window.location.href);
            url.searchParams.set('form', formId);
            window.history.replaceState(null, '', url);
            content.replaceChildren(settingsReview(review));
        } else {
            await refresh();
        }
    }), row(settingsDecision('Decline', 'deny', request.id, 'Settings changes declined.', true)));
    return item;
}

function settingsReview(request) {
    const item = card('Approve settings changes');
    item.classList.add('settings-review');
    for (const change of request.content.changes) {
        const values = node('dl', undefined, 'settings-changes');
        values.append(node('dt', 'Current'), node('dd', change.before),
            node('dt', 'Proposed'), node('dd', change.after));
        item.append(node('h4', change.label), values);
    }
    item.append(row(
        settingsDecision('Approve changes', 'approve-settings', request.id, 'Settings updated.'),
        settingsDecision('Decline', 'deny', request.id, 'Settings changes declined.', true)
    ));
    return item;
}

function settingsDecision(label, name, id, message, quiet = false) {
    if (!formId) return action(label, name, {id}, message, quiet);
    return button(label, async () => {
        await api(name, {id});
        finishFocusedForm(message);
    }, quiet);
}

function finishFocusedForm(title, message) {
    generation++;
    const done = card(title, message);
    done.append(button('Back to chat', () => telegram?.close()));
    content.replaceChildren(done);
    refreshButton.hidden = true;
}

function syncColorScheme() {
    const scheme = telegram?.colorScheme;
    if (scheme === 'light' || scheme === 'dark') document.documentElement.style.colorScheme = scheme;
}

if (formId) {
    shell.classList.add('narrow');
    content.setAttribute('aria-label', 'Form');
}
window.addEventListener('pagehide', closeOnboarding);
window.addEventListener('pageshow', event => {
    if (event.persisted && !state && !formId) void refresh();
});
refreshButton.addEventListener('click', () => refresh());
sourceLink.addEventListener('click', event => {
    if (!telegram?.initData || !telegram.openLink || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    telegram.openLink(sourceLink.href);
    event.preventDefault();
});
for (const item of document.querySelectorAll('#tabs button')) {
    if (item.dataset.tab === tab) item.setAttribute('aria-current', 'page');
    else item.removeAttribute('aria-current');
    item.addEventListener('click', () => setTab(item.dataset.tab));
}
syncColorScheme();
telegram?.onEvent?.('themeChanged', syncColorScheme);
if (telegram?.isVersionAtLeast?.('6.1')) {
    telegram.setHeaderColor('secondary_bg_color');
    telegram.setBackgroundColor('secondary_bg_color');
}
telegram?.ready();
refresh();
