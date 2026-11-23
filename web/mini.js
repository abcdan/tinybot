import { action, button, card, field, form, node, row } from './mini-ui.js';
import { agentForm, agents } from './mini-agents.js';
import { audit } from './mini-audit.js';
import { connectors } from './mini-connectors.js';
import { browser, schedules, model } from './mini-settings.js';

export const telegram = window.Telegram?.WebApp;
export const content = document.querySelector('#view');
const notice = document.querySelector('#status');
const refreshButton = document.querySelector('#refresh');
const tabs = document.querySelector('#tabs');
const browserTab = tabs.querySelector('[data-tab="browser"]');
const sourceLink = document.querySelector('#source-link');
const formId = new URLSearchParams(window.location.search).get('form');
let state;
let tab = 'home';
let generation = 0;

export function show(text, error = false) {
    notice.textContent = text;
    notice.hidden = !text;
    notice.classList.toggle('error', error);
}

export async function api(action, data = {}) {
    if (!telegram?.initData) throw new Error('Open this app from your TinyBot Telegram chat.');
    const response = await fetch('/api/mini', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({init: telegram.initData, action, data})
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'The request did not complete.');
    return result;
}

export async function refresh() {
    const sequence = ++generation;
    try {
        const result = formId ? await api('get-form', {id: formId}) : await api('status');
        if (sequence !== generation) return;
        if (formId) {
            show('');
            const item = card('Answer a few questions', 'Your answers go to your Chief.');
            item.append(generatedForm(result));
            content.replaceChildren(item);
        } else {
            state = result;
            browserTab.hidden = state.browser_proxy == null;
            if (tab === 'browser' && browserTab.hidden) setTab('home');
            else render();
        }
    } catch (error) {
        if (sequence === generation) {
            content.replaceChildren();
            show(error.message, true);
        }
    }
}

function setTab(next) {
    tab = next;
    for (const item of document.querySelectorAll('#tabs button')) {
        if (item.dataset.tab === tab) item.setAttribute('aria-current', 'page');
        else item.removeAttribute('aria-current');
    }
    render();
}

function render() {
    if (!state) return;
    content.replaceChildren();
    ({home, agents, connectors, browser, schedules, audit, model})[tab](state);
}

function home() {
    if (!state.provider) {
        const setup = card('Connect a model', 'Add an OpenRouter key or connect an eligible ChatGPT plan to give your Chief a model account.');
        setup.append(button('Set up model', () => setTab('model')));
        content.append(setup);
    }
    content.append(node('h2', 'Requests'));
    if (!state.requests.length) content.append(card(null, 'You’re all caught up.'));
    for (const request of state.requests) {
        const details = request.content;
        const title = { 'agent-spec': 'Configure a helper', form: 'Enter form details', 'form-submit': 'Submit website form', elicitation: 'Answer a few questions' }[request.kind];
        const item = card(details.operation || title, details.destination);
        if (request.kind === 'agent-spec') {
            item.append(agentForm(null, details, request.id));
            item.append(row(action('Decline', 'deny', {id: request.id}, 'Request declined.', true)));
        } else if (request.kind === 'form' || request.kind === 'elicitation') {
            if (request.kind === 'elicitation') item.append(node('p', 'Your answers go to your Chief.'));
            item.append(generatedForm(request), row(action('Decline', 'deny', {id: request.id}, 'Request declined.', true)));
        } else {
            if (details.preview || details.details) item.append(node('pre', details.preview || details.details));
            item.append(row(action('Approve', 'approve', {id: request.id}, 'Approved. TinyBot will continue.'),
                action('Decline', 'deny', {id: request.id}, 'Request declined.', true)));
        }
        content.append(item);
    }
    content.append(node('h2', 'Tasks'));
    if (!state.tasks.length) content.append(card(null, 'Send a message to your Chief in Telegram to start a task.'));
    for (const task of state.tasks) {
        const item = card(task.title, task.state.replaceAll('-', ' '));
        if (task.id !== state.active_task && task.state !== 'cancelled') {
            item.append(action('Switch to task', 'select-task', {task: task.id}, 'Send your next message in Telegram.', true));
        } else if (task.id === state.active_task) item.append(node('span', 'Current Chief task', 'meta'));
        content.append(item);
    }
}

function generatedForm(request) {
    const inputs = request.content.fields.map(spec => {
        if (spec.type === 'checkbox') {
            const label = node('label');
            const control = node('input');
            control.type = 'checkbox';
            control.name = spec.name;
            control.value = 'true';
            control.required = spec.required;
            label.append(control, document.createTextNode(spec.label));
            if (spec.hint) label.append(node('span', spec.hint, 'field-hint'));
            return label;
        }
        const choices = spec.type === 'select' ? [['', 'Choose an option'], ...spec.options.map(value => [value, value])] : undefined;
        return field(spec.name, spec.label, {
            type: spec.private && spec.type === 'text' ? 'password' : spec.type,
            choices, required: spec.required, max: 4096, hint: spec.hint
        });
    });
    const website = request.kind === 'form';
    const label = website ? 'Continue to review' : 'Send answers';
    return form(label, inputs, async (values, element) => {
        await api(website ? 'submit-form' : 'answer-form', {id: request.id, values});
        element.reset();
        if (formId) {
            const done = card('Answers sent', 'TinyBot will continue in your chat.');
            done.append(button('Back to chat', () => telegram?.close()));
            content.replaceChildren(done);
            refreshButton.hidden = true;
            return;
        }
        await refresh();
        show(website ? 'Details received. Review the submission below.' : 'Answers sent. TinyBot will continue.');
    });
}

function syncColorScheme() {
    const scheme = telegram?.colorScheme;
    if (scheme === 'light' || scheme === 'dark') document.documentElement.style.colorScheme = scheme;
}

tabs.hidden = Boolean(formId);
if (formId) {
    content.closest('main').classList.add('narrow');
    content.setAttribute('aria-label', 'Form');
}
refreshButton.addEventListener('click', () => refresh());
sourceLink.addEventListener('click', event => {
    if (!telegram?.initData || !telegram.openLink || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    telegram.openLink(sourceLink.href);
    event.preventDefault();
});
for (const item of document.querySelectorAll('#tabs button')) {
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
