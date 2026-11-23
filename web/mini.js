import { action, button, card, field, form, node, row } from './mini-ui.js';
import { agentForm, agents } from './mini-agents.js';
import { audit } from './mini-audit.js';
import { connectors } from './mini-connectors.js';
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
const sourceLink = document.querySelector('#source-link');
const footer = sourceLink.closest('footer');
const formId = new URLSearchParams(window.location.search).get('form');
const openInTelegram = 'Open TinyBot from its Telegram profile or tap Settings & forms in the bot chat.';
let state;
let tab = 'home';
let generation = 0;
let opened = false;
let disposeOnboarding;

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
            const item = card('Answer a few questions', 'Your answers go to your Chief.');
            item.append(generatedForm(result));
            content.replaceChildren(item);
        } else {
            state = result;
            chiefLabel.textContent = state.chief_name || 'Your Chief';
            browserTab.hidden = state.browser_proxy == null;
            if (tab === 'browser' && browserTab.hidden) setTab('home');
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
    ({home, agents, memories, connectors, mail, browser, schedules, audit, model})[tab](state);
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
        } else if (request.kind === 'email-send') {
            const data = {id: request.id};
            item.append(node('pre', details.explanation), row(
                action('Approve once', 'approve', data, 'Send approved.'),
                action('Deny', 'deny', data, 'Send denied.', true)
            ));
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
