import { action, button, card, field, form, node, row } from './mini-ui.js';
import { agentForm, agents } from './mini-agents.js';
import { audit } from './mini-audit.js';
import { connectors } from './mini-connectors.js';
import { invites } from './mini-invites.js';
import { mail } from './mini-mail.js';
import { memories } from './mini-memory.js';
import { onboarding } from './mini-onboarding.js';
import { browser, schedules, model } from './mini-settings.js';
import { settingsHistory as renderSettingsHistory } from './mini-settings-history.js';
import { tasks } from './mini-tasks.js';

export const telegram = window.Telegram?.WebApp;
export const content = document.querySelector('#view');
const notice = document.querySelector('#status');
const refreshButton = document.querySelector('#refresh');
const chiefLabel = document.querySelector('#chief-label');
const shell = content.closest('main');
const tabs = document.querySelector('#tabs');
const sourceLink = document.querySelector('#source-link');
const footer = sourceLink.closest('footer');
const backButton = telegram?.BackButton;
const params = new URLSearchParams(window.location.search);
let formId = params.get('form');
const openInTelegram = 'Open TinyBot from its Telegram profile or tap Settings & forms in the bot chat.';
let state;
let tab = params.get('connect') === 'docblock' ? 'connectors' : 'home';
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
        footer.hidden = true;
        if (!opened) {
            opened = true;
            openFullscreen();
        }
        show('');
        if (needsOnboarding) {
            backButton?.hide?.();
            state = undefined;
            openOnboarding(result);
            return;
        }
        closeOnboarding();
        if (formId) {
            backButton?.hide?.();
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
            chiefLabel.textContent = state.chief_name || '';
            chiefLabel.hidden = !state.chief_name;
            if (!tabAvailable(tab)) setTab('home');
            else render(animate);
        }
    } catch (error) {
        if (sequence === generation) {
            closeOnboarding();
            state = undefined;
            tabs.hidden = true;
            backButton?.hide?.();
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

const mainTabs = {home: 'Tasks', agents: 'Helpers', audit: 'Activity', settings: 'Settings'};
const settingsGroups = [
    ['Personal', [
        ['model', 'Model', 'Task and image models'],
        ['memories', 'Memory', 'Personal and shared notes'],
        ['settingsHistory', 'Settings history', 'Changes and rollbacks'],
    ]],
    ['Services', [
        ['connectors', 'Connections', 'Docblock and external tools'],
        ['mail', 'Email', 'Accounts and watched folders'],
        ['browser', 'Browser', 'Proxy and connection'],
    ]],
    ['Automation and access', [
        ['schedules', 'Schedules', 'Reminders and routines'],
        ['invites', 'Invites', 'Guest access'],
    ]],
];

function tabAvailable(name) {
    return (name !== 'browser' || state.browser_proxy != null) &&
        (name !== 'invites' || state.operator === true);
}

function setTab(next) {
    const changed = tab !== next;
    tab = next;
    render(changed);
    if (changed) {
        window.scrollTo(0, 0);
        content.focus({preventScroll: true});
    }
}

function returnToSettings() {
    const destination = tab;
    setTab('settings');
    [...content.querySelectorAll('[data-tab]')]
        .find(item => item.dataset.tab === destination)?.focus({preventScroll: true});
}

function render(animate = false) {
    if (!state) return;
    const mainTab = Object.hasOwn(mainTabs, tab) ? tab : 'settings';
    for (const item of tabs.querySelectorAll('button')) {
        if (item.dataset.tab === mainTab) item.setAttribute('aria-current', 'page');
        else item.removeAttribute('aria-current');
    }
    footer.hidden = tab !== 'settings';
    content.setAttribute('aria-label', mainTabs[mainTab]);
    content.replaceChildren();
    if (mainTab !== tab) {
        backButton?.show?.();
        const back = button('Back to settings', returnToSettings, true);
        back.classList.add('back-link');
        content.append(back);
    } else backButton?.hide?.();
    ({home, agents, audit, settings, memories, connectors, mail, browser, schedules, invites, model, settingsHistory})[tab](state);
    if (animate && !window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
        content.getAnimations?.().forEach(animation => animation.cancel());
        content.animate?.([{opacity: 0, transform: 'translateY(4px)'}, {opacity: 1, transform: 'translateY(0)'}],
            {duration: 160, easing: 'ease-out'});
    }
}

function settings() {
    const panel = node('div', undefined, 'settings-hub');
    panel.append(node('h2', 'Settings', 'page-title'));
    for (const [title, destinations] of settingsGroups) {
        const group = node('section', undefined, 'settings-group');
        group.append(node('h3', title, 'settings-group-title'));
        const list = node('div', undefined, 'settings-menu');
        for (const [id, label, description] of destinations) {
            if (!tabAvailable(id)) continue;
            const control = button('', () => setTab(id));
            control.dataset.tab = id;
            control.classList.add('settings-destination');
            control.setAttribute('aria-label', label);
            const text = node('span', undefined, 'settings-destination-text');
            text.append(node('strong', label), node('small', description));
            const chevron = node('span', undefined, 'disclosure-chevron');
            chevron.setAttribute('aria-hidden', 'true');
            control.append(text, chevron);
            list.append(control);
        }
        group.append(list);
        panel.append(group);
    }
    content.append(panel);
}

function home() {
    if (!state.provider) {
        const setup = card('Connect a model', 'Add an OpenRouter key or connect an eligible ChatGPT plan to give your Chief a model account.');
        setup.append(button('Set up model', () => setTab('model')));
        content.append(setup);
    }
    tasks(state, requestCard, () => setTab('model'));
}

function settingsHistory(state) {
    renderSettingsHistory(state, settingsReview);
}

function requestCard(request) {
    if (request.kind === 'settings-form') return settingsForm(request);
    if (request.kind === 'settings-approve') return settingsReview(request);
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
for (const item of tabs.querySelectorAll('button')) {
    item.addEventListener('click', () => setTab(item.dataset.tab));
}
backButton?.onClick?.(() => {
    if (state && !formId && !Object.hasOwn(mainTabs, tab)) returnToSettings();
});
syncColorScheme();
telegram?.onEvent?.('themeChanged', syncColorScheme);
if (telegram?.isVersionAtLeast?.('6.1')) {
    telegram.setHeaderColor('secondary_bg_color');
    telegram.setBackgroundColor('secondary_bg_color');
}
telegram?.ready();
refresh();
