import { api, content } from './mini.js';
import { focusedAudit } from './mini-audit.js';
import { action, button, card, field, form, node, row } from './mini-ui.js';

let view;
let selectedTask = new URLSearchParams(window.location.search).get('task');

const labels = {
    idle: 'Ready for a follow-up', running: 'Working', waiting: 'Waiting for your response',
    setup: 'Waiting for model setup', cancelled: 'Stopped', blocked: 'Stopped with an error',
    uncertain: 'Check the destination before continuing', queued: 'Queued', completed: 'Completed'
};
const stoppable = ['queued', 'running', 'waiting', 'setup'];
const needsYou = ['waiting', 'setup', 'blocked', 'uncertain'];

export function tasks(state, requestCard, openModel) {
    view ??= state.requests.length && !selectedTask ? 'needs-you' : 'open';
    const panel = node('div', undefined, 'tasks-panel');
    content.append(panel);
    const dateFormat = new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium', timeStyle: 'short', timeZone: state.time_zone,
    });
    const date = timestamp => dateFormat.format(new Date(timestamp * 1000));

    function select(taskId) {
        selectedTask = taskId;
        const url = new URL(window.location.href);
        if (taskId) url.searchParams.set('task', taskId);
        else url.searchParams.delete('task');
        window.history.replaceState(null, '', url);
        render();
    }

    function render() {
        panel.replaceChildren();
        if (selectedTask) {
            void detail(selectedTask);
            return;
        }
        panel.append(node('h2', 'Tasks'));
        const views = row(...[
            ['open', 'Open'], ['needs-you', 'Needs you'], ['completed', 'Completed tasks'],
        ].map(([id, label]) => {
            const control = button(label, () => {
                view = id;
                render();
                panel.querySelector('[aria-pressed="true"]')?.focus();
            }, id !== view);
            control.setAttribute('aria-pressed', String(id === view));
            if (id === 'needs-you' && state.requests.length) {
                const count = node('span', ` ${state.requests.length}`, 'task-count');
                count.setAttribute('aria-hidden', 'true');
                control.setAttribute('aria-label', label);
                control.setAttribute('aria-description', `${state.requests.length} pending requests`);
                control.append(count);
            }
            return control;
        }));
        views.classList.add('task-views');
        views.setAttribute('role', 'group');
        views.setAttribute('aria-label', 'Task views');
        panel.append(views);
        if (view === 'needs-you') inbox();
        else if (view === 'completed') completed();
        else openTasks();
    }

    function openTasks() {
        const results = node('div');
        let generation = 0;
        async function search(query, offset = 0) {
            const sequence = ++generation;
            const page = await api('search-tasks', {query, offset});
            if (sequence !== generation || !results.isConnected) return;
            if (!offset) results.replaceChildren();
            taskPage(results, page, next => search(query, next), !offset && 'No matching tasks.');
        }
        const finder = form('Search tasks', [field('query', 'Search retained messages and tasks', {
            max: 200, required: false, type: 'search'
        })], ({query}) => search(query.trim()));
        const tasks = state.tasks.filter(task => !['completed', 'cancelled'].includes(task.state));
        if (!tasks.length) results.append(card(null, state.tasks.length ? 'No open tasks.' :
            'Send a message to your Chief in Telegram to start a task.'));
        results.append(...tasks.map(taskCard));
        panel.append(finder, results);
    }

    function completed() {
        const results = node('div');
        const status = node('p', 'Loading completed tasks…', 'meta');
        status.setAttribute('role', 'status');
        panel.append(status, results);
        async function load(offset = 0) {
            const page = await api('search-tasks', {query: '', offset, completed_only: true});
            if (!results.isConnected) return;
            status.hidden = true;
            taskPage(results, page, load, !offset && 'No completed tasks yet.');
        }
        load().catch(error => {
            if (!results.isConnected) return;
            status.textContent = error.message;
            const retry = button('Retry loading completed tasks', async () => {
                await load();
                retry.remove();
            }, true);
            results.append(retry);
        });
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

    function inbox() {
        const pending = new Set(state.requests.map(request => request.task));
        const tasks = state.tasks.filter(task => needsYou.includes(task.state) && !pending.has(task.id));
        if (!state.requests.length && !tasks.length) panel.append(card(null, 'You’re all caught up.'));
        for (const request of state.requests) {
            const task = state.tasks.find(task => task.id === request.task);
            panel.append(review(request, task));
        }
        panel.append(...tasks.map(taskCard));
    }

    function review(request, task) {
        const item = requestCard(request);
        const metadata = node('div', undefined, 'task-review-meta');
        if (task) metadata.append(button(task.title, () => select(task.id), true));
        if (Number.isFinite(request.expires)) metadata.append(node('span', `Expires ${date(request.expires)}`, 'meta'));
        if (metadata.childElementCount) item.append(metadata);
        return item;
    }

    function taskCard(task) {
        const item = card(task.title, labels[task.state] || task.state);
        item.classList.add('task-card');
        if (Number.isFinite(task.updated)) item.append(node('span', `Updated ${date(task.updated)}`, 'meta'));
        if (task.id === state.active_task) item.append(node('span', 'Current Chief task', 'meta'));
        item.append(row(button(task.state === 'waiting' ? 'Review requests' : 'Open task', () => select(task.id), true), ...controls(task)));
        return item;
    }

    function controls(task, executing = false) {
        if (executing) return [];
        const buttons = [];
        const input = {task: task.id};
        if (Number.isSafeInteger(task.revision)) input.revision = task.revision;
        if (task.state === 'completed') {
            buttons.push(action('Reopen', 'select-task', input, 'Task reopened. Send your next message in Telegram.', true));
        } else if (task.id !== state.active_task && task.state !== 'cancelled') {
            buttons.push(action('Continue', 'select-task', input, 'Send your next message in Telegram.', true));
        }
        if (Number.isSafeInteger(task.revision)) {
            if ([...stoppable, 'idle', 'blocked'].includes(task.state)) {
                buttons.push(action('Mark complete', 'complete-task', input, 'Task marked complete.', true));
            }
            if (stoppable.includes(task.state)) buttons.push(action('Stop', 'stop-task', input, 'Task stopped.', true));
        }
        return buttons;
    }

    async function detail(id) {
        const loading = node('p', 'Loading task…', 'meta');
        loading.setAttribute('role', 'status');
        const body = node('div', undefined, 'task-detail');
        body.append(loading);
        panel.append(button('Back to tasks', () => select(null), true), body);
        try {
            const data = await api('task-detail', {task: id});
            if (!body.isConnected) return;
            const {task, reply, audit, requests} = data;
            const status = card(task.title, labels[task.state] || task.state);
            if (Number.isFinite(task.updated)) status.append(node('span', `Updated ${date(task.updated)}`, 'meta'));
            if (task.id === state.active_task) status.append(node('span', 'Current Chief task', 'meta'));
            if (data.executing) status.append(node('p', 'An action is in progress. Refresh to see its result.'));
            else if (data.failure) status.append(node('p', data.failure));
            else {
                const guidance = {
                    completed: 'Reopen this task to continue in Telegram.', idle: 'Send a follow-up in Telegram to continue.',
                    waiting: requests.length ? '' : 'Check the request in Telegram.',
                    uncertain: 'Verify the result at its destination, then send /reconcile in Telegram.',
                }[task.state];
                if (guidance) status.append(node('p', guidance));
            }
            const buttons = controls(task, data.executing);
            if (task.state === 'setup') buttons.push(button('Set up model', openModel, true));
            if (audit) buttons.push(button('View audit', () => focusedAudit(panel, state, {
                ...audit, message: task.title,
            }, render), true));
            if (buttons.length) status.append(row(...buttons));
            body.replaceChildren(status);
            if (requests.length) body.append(node('h2', 'Needs you'), ...requests.map(request => review(request)));
            if (data.last_confirmed) body.append(card('Last confirmed step', data.last_confirmed));
            if (reply) {
                const latest = card('Latest reply');
                latest.append(node('div', reply.text, 'task-reply'));
                if (reply.truncated) latest.append(node('p', 'Open the Telegram conversation for the full reply.', 'meta'));
                body.append(latest);
            }
        } catch (error) {
            if (!body.isConnected) return;
            const item = card(null, error.message);
            if (error.status !== 404) item.append(button('Retry loading task', render, true));
            body.replaceChildren(item);
        }
    }

    render();
}
