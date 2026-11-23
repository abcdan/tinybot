import { button, node, row } from './mini-ui.js';
import { api, content } from './mini.js';

export function audit(state) {
    const dates = new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium', timeStyle: 'medium', timeZone: state.time_zone
    });
    const sessions = node('div');
    const status = node('p', '', 'meta');
    status.setAttribute('role', 'status');
    const more = button('Load older messages', load, true);
    let before = null;

    async function load() {
        status.hidden = false;
        status.textContent = 'Loading audit log…';
        more.hidden = true;
        try {
            const page = await api('audit-sessions', before === null ? {} : {before});
            sessions.append(...page.sessions.map(run => auditRun(run, dates)));
            before = page.next_before ?? null;
            status.textContent = 'There are no message runs in the last 7 days.';
            status.hidden = sessions.childElementCount > 0;
            more.hidden = before === null;
            more.textContent = 'Load older messages';
        } catch (error) {
            status.textContent = error.message;
            more.textContent = 'Retry loading audit log';
            more.hidden = false;
        }
    }

    content.append(node('h2', 'Audit log'), node('p', 'Message runs and their model and tool calls are kept for 7 days. Sensitive payloads are summarized.', 'meta'),
        sessions, status, more);
    void load();
}

function auditRun(run, dates) {
    const card = node('details', undefined, 'card audit-run');
    const title = node('summary');
    const summary = node('span', describe(run, dates), 'meta');
    title.append(node('span', run.message || 'Message'), summary);
    const finished = node('p', '', 'meta');
    const timeline = node('ol', undefined, 'audit-timeline');
    timeline.setAttribute('role', 'list');
    const status = node('p', 'Loading run entries…', 'meta');
    status.setAttribute('role', 'status');
    const more = button('Load more entries', () => load(true), true);
    const refresh = button('Refresh entries', () => load(), true);
    const actions = row(more, refresh);
    finished.hidden = true;
    actions.hidden = true;
    card.append(title, finished, timeline, status, actions);
    let after = null;
    let loaded = false;
    let loading = false;

    async function load(nextPage = false) {
        if (loading) return;
        loading = true;
        status.hidden = false;
        status.textContent = 'Loading run entries…';
        actions.hidden = true;
        try {
            const result = await api('audit-session', nextPage ? {id: run.id, after} : {id: run.id});
            summary.textContent = describe(result.session, dates);
            finished.hidden = result.session.finished_at == null;
            finished.textContent = finished.hidden ? '' : 'Finished ' + date(result.session.finished_at, dates);
            const entries = result.entries.map(item => auditEntry(item, dates));
            if (nextPage) timeline.append(...entries);
            else timeline.replaceChildren(...entries);
            after = result.next_after ?? null;
            status.textContent = 'No model or tool calls have been recorded for this message.';
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

    card.addEventListener('toggle', () => {
        if (card.open && !loaded) void load();
    });
    return card;
}

function auditEntry(item, dates) {
    const entry = node('li');
    const title = [label(item.kind), item.name].filter(Boolean).join(' · ');
    const attempt = item.attempt > 0 ? 'Attempt ' + item.attempt : '';
    const timing = [date(item.at, dates), label(item.status), attempt].filter(Boolean).join(' · ');
    entry.append(node('strong', title), node('p', timing, 'meta'));
    if (item.detail != null) {
        const parts = typeof item.detail === 'object' && !Array.isArray(item.detail)
            ? Object.entries(item.detail)
            : [['details', item.detail]];
        for (const [key, value] of parts) {
            entry.append(node('div', label(key), 'meta'),
                node('pre', typeof value === 'string' ? value : JSON.stringify(value, null, 2)));
        }
    }
    return entry;
}

function describe(run, dates) {
    return [run.agent || 'Chief', date(run.started_at, dates), label(run.status),
        run.entry_count === 1 ? '1 entry' : run.entry_count + ' entries'].filter(Boolean).join(' · ');
}

function date(timestamp, dates) {
    return timestamp == null ? '' : dates.format(new Date(timestamp * 1000));
}

function label(value) {
    if (!value) return '';
    return value.charAt(0).toUpperCase() + value.slice(1).replace(/[_-]/g, ' ');
}
