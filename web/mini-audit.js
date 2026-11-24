import { button, card, node, row } from './mini-ui.js';
import { api, content, show } from './mini.js';

const numbers = new Intl.NumberFormat();
const dollars = new Intl.NumberFormat(undefined, {
    style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 8,
});
const providerNames = new Map([['codex', 'Codex'], ['openrouter', 'OpenRouter']]);
const statusTones = new Map([
    ['completed', 'success'], ['allow', 'success'], ['approved', 'success'],
    ['failed', 'error'], ['deny', 'error'], ['denied', 'error'],
    ['waiting', 'warning'], ['waiting_approval', 'warning'], ['paused', 'warning'],
    ['uncertain', 'warning'], ['interrupted', 'warning'],
]);

export function audit(state) {
    const timeZone = state.time_zone;
    const dates = {
        message: new Intl.DateTimeFormat(undefined, {month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit', timeZone}),
        time: new Intl.DateTimeFormat(undefined, {timeStyle: 'medium', timeZone}),
        full: new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeStyle: 'medium', timeZone}),
    };
    const page = node('div', undefined, 'audit-page');
    const usage = usagePanel('Last 7 days', 'h3', 'audit-usage card');
    const sessions = card();
    sessions.classList.add('audit-runs');
    sessions.hidden = true;
    const status = node('p', '', 'meta');
    status.setAttribute('role', 'status');
    const more = button('Load older messages', load, true);
    let before = null;

    async function load() {
        status.hidden = false;
        status.textContent = 'Loading audit log…';
        more.hidden = true;
        try {
            const result = await api('audit-sessions', before === null ? {} : {before});
            if (before === null) usage.update(result.usage);
            sessions.append(...result.sessions.map(run => auditRun(run, dates)));
            sessions.hidden = sessions.childElementCount === 0;
            before = result.next_before ?? null;
            status.textContent = 'No messages in the last 7 days.';
            status.hidden = !sessions.hidden;
            more.hidden = before === null;
            more.textContent = 'Load older messages';
        } catch (error) {
            status.textContent = error.message;
            more.textContent = 'Retry loading audit log';
            more.hidden = false;
        }
    }

    page.append(node('h2', 'Audit log', 'audit-page-title'), usage.section,
        node('h3', 'Recent messages', 'audit-section-title'), sessions, status, more);
    content.append(page);
    void load();
}

function auditRun(run, dates) {
    const group = node('details', undefined, 'audit-run');
    const title = node('summary', undefined, 'audit-run-summary');
    const titleCopy = node('span', undefined, 'audit-run-copy');
    const message = node('span', run.message || 'Message', 'audit-run-message');
    const summary = node('span', undefined, 'audit-run-meta meta');
    const statusLabel = node('span', undefined, 'audit-status');
    titleCopy.append(message, summary);
    title.append(titleCopy, statusLabel);

    const body = node('div', undefined, 'audit-run-body');
    const usage = usagePanel('Run usage', 'h4', 'audit-run-usage');
    const finished = node('p', undefined, 'audit-run-finished meta');
    const heading = node('h4', 'Activity', 'audit-timeline-heading');
    const timeline = node('ol', undefined, 'audit-timeline');
    timeline.setAttribute('role', 'list');
    const status = node('p', 'Loading run entries…', 'meta');
    status.setAttribute('role', 'status');
    const more = button('Load more entries', () => load(true), true);
    const refresh = button('Refresh entries', () => load(), true);
    const actions = row(more, refresh);
    actions.hidden = true;
    body.append(usage.section, auditExport(group, run.id), heading, finished, timeline, status, actions);
    group.append(title, body);
    updateTitle(run);
    let after = null;
    let loaded = false;
    let loading = false;

    function updateTitle(session) {
        const start = timestamp(session.started_at, dates.message, dates.full);
        summary.replaceChildren(document.createTextNode(`${session.agent || 'Chief'} · `), start);
        finished.hidden = session.finished_at == null;
        if (!finished.hidden) finished.replaceChildren('Finished ', timestamp(session.finished_at, dates.time, dates.full));
        updateStatus(statusLabel, session.status);
    }

    async function load(nextPage = false) {
        if (loading) return;
        loading = true;
        status.hidden = false;
        status.textContent = 'Loading run entries…';
        actions.hidden = true;
        try {
            const result = await api('audit-session', nextPage ? {id: run.id, after} : {id: run.id});
            updateTitle(result.session);
            usage.update(result.usage);
            const entries = result.entries.map(item => auditEntry(item, dates));
            if (nextPage) timeline.append(...entries);
            else timeline.replaceChildren(...entries);
            after = result.next_after ?? null;
            status.textContent = 'No activity recorded for this message yet.';
            status.hidden = timeline.childElementCount > 0;
            more.textContent = 'Load more entries';
            refresh.textContent = 'Refresh entries';
            loaded = true;
        } catch (error) {
            status.textContent = error.message;
            if (nextPage) more.textContent = 'Retry loading more entries';
            else refresh.textContent = 'Retry loading entries';
        } finally {
            more.hidden = after === null;
            actions.hidden = false;
            loading = false;
        }
    }

    group.addEventListener('toggle', () => {
        if (group.open && !loaded) void load();
    });
    return group;
}

function auditExport(group, id) {
    const section = node('div', undefined, 'audit-export');
    const output = node('textarea');
    output.readOnly = true;
    output.spellcheck = false;
    output.setAttribute('aria-label', 'Audit JSON');

    function selectJson() {
        output.hidden = false;
        output.focus();
        output.select();
    }

    const copy = button('Copy JSON', async () => {
        try {
            await navigator.clipboard.writeText(output.value);
            show('Audit JSON copied.');
        } catch {
            selectJson();
        }
    }, true);

    function clear() {
        output.value = '';
        output.hidden = true;
        copy.hidden = true;
    }

    const download = button('Export JSON', async () => {
        clear();
        const snapshot = await api('audit-export', {id});
        if (!group.open || !group.isConnected) return;
        output.value = JSON.stringify(snapshot, null, 2) + '\n';
        copy.hidden = false;
        try {
            downloadJson(output.value, `tinybot-audit-${id}.json`);
        } catch {
            selectJson();
        }
    }, true);

    clear();
    group.addEventListener('toggle', () => { if (!group.open) clear(); });
    section.append(row(download, copy), output);
    return section;
}

function downloadJson(json, filename) {
    const url = URL.createObjectURL(new Blob([json], {type: 'application/json'}));
    const link = node('a');
    link.href = url;
    link.download = filename;
    link.hidden = true;
    document.body.append(link);
    try {
        link.click();
    } finally {
        link.remove();
        setTimeout(() => URL.revokeObjectURL(url), 30_000);
    }
}

function usagePanel(title, headingTag, className) {
    const section = node('section', undefined, className);
    const overview = node('div');
    const disclosure = node('details', undefined, 'audit-disclosure');
    const breakdown = node('div', undefined, 'audit-breakdown');
    disclosure.append(node('summary', 'Usage breakdown'), breakdown);
    section.append(node(headingTag, title, 'audit-usage-heading'), overview, disclosure);
    section.hidden = true;

    return {section, update(usage) {
        section.hidden = !usage;
        if (!usage) return;
        const {total, providers, models, legacy_entries: legacyEntries} = usage;
        disclosure.hidden = total.calls === 0;
        if (total.calls === 0) {
            const message = legacyEntries > 0 ? 'Usage wasn’t recorded for these calls.' : 'No model calls recorded.';
            overview.replaceChildren(node('p', message, 'audit-usage-note meta'));
            return;
        }
        overview.replaceChildren(metrics([
            ['Reported cost', formatCost(total.cost_usd)],
            ['Total tokens', formatTokens(total.total_tokens)],
            ['Model calls', numbers.format(total.calls)],
        ], 'audit-metrics audit-metrics-primary'));
        const note = usageNote(total, legacyEntries);
        if (note) overview.append(node('p', note, 'audit-usage-note meta'));
        breakdown.replaceChildren(metrics([
            ['Input tokens', formatTokens(total.input_tokens)],
            ['Output tokens', formatTokens(total.output_tokens)],
        ]), ...providers.map(provider => providerUsage(provider, models)));
    }};
}

function providerUsage(provider, models) {
    const section = node('section', undefined, 'audit-provider');
    const name = providerNames.get(provider.provider) ?? provider.provider;
    section.append(node('h4', name), usageMetrics(provider), usageCaption(provider));
    const list = node('ul', undefined, 'audit-models');
    list.setAttribute('role', 'list');
    for (const model of models.filter(model => model.provider === provider.provider)) {
        const item = node('li');
        item.append(node('strong', model.model), usageMetrics(model), usageCaption(model));
        list.append(item);
    }
    if (list.childElementCount) section.append(list);
    return section;
}

function usageMetrics(usage) {
    const fields = [
        ['Cost (USD)', formatCost(usage.cost_usd)],
        ['Input tokens', formatTokens(usage.input_tokens)],
        ['Output tokens', formatTokens(usage.output_tokens)],
        ['Total tokens', formatTokens(usage.total_tokens)],
    ];
    if (usage.reasoning_tokens != null) {
        fields.push(['Reasoning tokens (included in output)', formatTokens(usage.reasoning_tokens)]);
    }
    return metrics(fields);
}

function metrics(items, className = 'audit-metrics') {
    const list = node('dl', undefined, className);
    for (const [name, value] of items) {
        const item = node('div');
        item.append(node('dt', name), node('dd', value));
        list.append(item);
    }
    return list;
}

function usageNote({calls, token_reported_calls: tokenCalls, cost_reported_calls: costCalls}, legacyEntries = 0) {
    const notes = [];
    if (costCalls > 0 && costCalls < calls) notes.push(`Cost reported for ${numbers.format(costCalls)} of ${numbers.format(calls)} calls.`);
    if (tokenCalls > 0 && tokenCalls < calls) notes.push(`Tokens reported for ${numbers.format(tokenCalls)} of ${numbers.format(calls)} calls.`);
    if (legacyEntries > 0) notes.push('Earlier calls may be missing usage data.');
    return notes.join(' ');
}

function usageCaption(usage) {
    const calls = `${numbers.format(usage.calls)} ${usage.calls === 1 ? 'call' : 'calls'}`;
    return node('p', [calls, usageNote(usage)].filter(Boolean).join(' · '), 'audit-usage-note meta');
}

function formatTokens(value) {
    return value == null ? 'Not reported' : numbers.format(value);
}

function formatCost(value) {
    if (value == null) return 'Not reported';
    if (value > 0 && value < 0.00000001) return '< ' + dollars.format(0.00000001);
    return dollars.format(value);
}

function auditEntry(item, dates) {
    const entry = node('li');
    const parts = detailParts(item.detail);
    const line = node(parts.length ? 'summary' : 'div', undefined, 'audit-entry-row');
    const title = node('span', undefined, 'audit-entry-copy');
    const name = node('span', item.name?.includes('/') ? item.name : readable(item.name || item.kind), 'audit-entry-name');
    name.title = item.name || '';
    const meta = node('span', `${readable(item.kind)} · `, 'audit-entry-meta meta');
    meta.append(timestamp(item.at, dates.time, dates.full));
    if (item.attempt > 1) meta.append(document.createTextNode(` · Attempt ${item.attempt}`));
    const status = node('span', undefined, 'audit-status');
    updateStatus(status, item.status);
    title.append(name, meta);
    line.append(title, status);
    if (parts.length) {
        const details = node('details', undefined, 'audit-entry');
        const body = node('div', undefined, 'audit-entry-body');
        const values = node('dl', undefined, 'audit-entry-details');
        for (const [key, value] of parts) {
            if (item.kind === 'model_call' && key === 'usage' && value != null) {
                body.append(usageMetrics(value));
            } else {
                const text = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
                const field = node('div');
                field.append(node('dt', readable(key)), node('dd', text));
                values.append(field);
            }
        }
        if (values.childElementCount) body.prepend(values);
        details.append(line, body);
        entry.append(details);
    } else {
        entry.append(line);
    }
    return entry;
}

function detailParts(detail) {
    if (detail == null) return [];
    if (typeof detail !== 'object' || Array.isArray(detail)) return [['Details', detail]];
    return Object.entries(detail).filter(([key]) => key !== 'usage_tracking');
}

function updateStatus(element, status) {
    element.textContent = readable(status);
    element.dataset.tone = statusTones.get(status) ?? 'neutral';
    element.hidden = !status;
}

function timestamp(seconds, formatter, full) {
    const time = node('time');
    if (seconds == null) return time;
    const date = new Date(seconds * 1000);
    time.dateTime = date.toISOString();
    time.textContent = formatter.format(date);
    time.title = full.format(date);
    return time;
}

function readable(value) {
    if (!value) return '';
    return value.charAt(0).toUpperCase() + value.slice(1).replace(/[_-]/g, ' ');
}
