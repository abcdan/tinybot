import { api, content, setBackAction, telegram } from './mini.js';
import { fileShelf } from './mini-artifacts.js';
import { focusedAudit } from './mini-audit.js';
import { saveToDocblock } from './mini-docblock-save.js';
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
    const list = node('div');
    const detailView = node('div');
    panel.append(list, detailView);
    content.append(panel);
    let listScroll = 0;
    let opener;
    const dateFormat = new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium', timeStyle: 'short', timeZone: state.time_zone,
    });
    const date = timestamp => dateFormat.format(new Date(timestamp * 1000));
    const telegramTask = state.telegram_task || state.tasks.find(task => task.id === state.active_task);
    const openItems = state.tasks.filter(task => !['completed', 'cancelled'].includes(task.state));

    function taskDates(task, detailed = true) {
        const dates = [];
        const updated = Number.isFinite(task.updated) && task.updated !== task.created_at;
        if (Number.isFinite(task.created_at) && (detailed || !updated)) dates.push(`Started ${date(task.created_at)}`);
        if (updated) dates.push(`Updated ${date(task.updated)}`);
        return dates.length ? node('p', dates.join(' · '), 'meta') : '';
    }

    function startNew(task, label = 'Start new task') {
        return action(label, 'start-new-task', {task: task.id, revision: task.revision},
            'Send a message in Telegram to start your new task.');
    }

    function emptyState(message) {
        return node('p', message, 'task-empty');
    }

    function select(taskId, control) {
        if (taskId && !selectedTask) {
            listScroll = window.scrollY;
            opener = control;
        }
        selectedTask = taskId;
        const url = new URL(window.location.href);
        if (taskId) url.searchParams.set('task', taskId);
        else url.searchParams.delete('task');
        window.history.replaceState(null, '', url);
        renderSelection();
        window.scrollTo(0, taskId ? 0 : listScroll);
        const target = taskId ? detailView.firstElementChild
            : opener?.isConnected ? opener : list.querySelector('h2');
        target?.focus({preventScroll: true});
    }

    function taskButton(label, id, quiet = true) {
        const control = button(label, () => select(id, control), quiet);
        return control;
    }

    function renderSelection() {
        list.hidden = Boolean(selectedTask);
        detailView.hidden = !selectedTask;
        detailView.replaceChildren();
        if (selectedTask) {
            setBackAction(() => select(null));
            void detail(selectedTask);
        } else {
            setBackAction();
            if (!list.hasChildNodes()) renderList();
        }
    }

    function renderList() {
        list.replaceChildren();
        const heading = node('div', undefined, 'task-heading');
        const hasTelegramTask = telegramTask && Number.isSafeInteger(telegramTask.revision);
        const create = hasTelegramTask ? startNew(telegramTask, 'New task')
            : button('New task', () => telegram?.close?.());
        const plus = node('span', '+', 'new-task-icon');
        plus.setAttribute('aria-hidden', 'true');
        create.classList.add('new-task');
        create.prepend(plus);
        const title = node('h2', 'Tasks', 'page-title');
        title.tabIndex = -1;
        heading.append(title, create);
        list.append(heading);
        let continuation;
        if (hasTelegramTask) {
            const selected = taskButton('', telegramTask.id, false);
            selected.classList.add('task-continuation');
            selected.setAttribute('aria-description', telegramTask.title);
            selected.hidden = view === 'open' && openItems.some(task => task.id === telegramTask.id);
            continuation = selected;
            selected.setAttribute('aria-label', 'Open selected task');
            const text = node('span', undefined, 'task-continuation-text');
            text.append(node('span', 'Next in Telegram', 'task-continuation-label'),
                node('strong', telegramTask.title));
            const chevron = node('span', undefined, 'disclosure-chevron');
            chevron.setAttribute('aria-hidden', 'true');
            selected.append(text, chevron);
            list.append(selected);
        }
        const views = node('div');
        views.append(...[
            ['open', 'Open'], ['needs-you', 'Needs you'], ['completed', 'Completed'],
        ].map(([id, label]) => {
            const control = button(label, () => {
                view = id;
                renderList();
                list.querySelector('[aria-pressed="true"]')?.focus();
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
        list.append(views);
        if (view === 'needs-you') inbox();
        else if (view === 'completed') completed();
        else openTasks(continuation);
    }

    function openTasks(continuation) {
        const results = node('div');
        let generation = 0;
        async function search(query, offset = 0) {
            const sequence = ++generation;
            const page = await api('search-tasks', {query, offset});
            if (sequence !== generation || !results.isConnected) return;
            if (!offset) results.replaceChildren();
            if (continuation && (!offset || page.items.some(task => task.id === telegramTask.id))) {
                continuation.hidden = page.items.some(task => task.id === telegramTask.id);
            }
            taskPage(results, page, next => search(query, next), !offset && 'No matching tasks.');
        }
        const finder = form('Search tasks', [field('query', 'Search retained messages and tasks', {
            max: 200, required: false, type: 'search'
        })], ({query}) => search(query.trim()));
        finder.classList.add('task-search');
        finder.querySelector('input').placeholder = 'Search tasks and messages';
        const submit = finder.querySelector('button');
        submit.setAttribute('aria-label', 'Search tasks');
        submit.title = 'Search tasks';
        submit.textContent = '';
        if (!openItems.length) results.append(emptyState(state.tasks.length ? 'No open tasks.' :
            'Send a message in Telegram to start a task.'));
        results.append(...openItems.map(taskCard));
        list.append(finder, results);
    }

    function completed() {
        const results = node('div');
        const status = node('p', 'Loading completed tasks…', 'meta');
        status.setAttribute('role', 'status');
        list.append(status, results);
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
        if (!page.items.length && empty) results.append(emptyState(empty));
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
        if (!state.requests.length && !tasks.length) list.append(emptyState('You’re all caught up.'));
        for (const request of state.requests) {
            const task = state.tasks.find(task => task.id === request.task);
            list.append(review(request, task));
        }
        list.append(...tasks.map(taskCard));
    }

    function review(request, task) {
        const item = requestCard(request);
        const metadata = node('div', undefined, 'task-review-meta');
        if (task) metadata.append(taskButton(task.title, task.id));
        if (Number.isFinite(request.expires)) metadata.append(node('span', `Expires ${date(request.expires)}`, 'meta'));
        if (metadata.childElementCount) item.append(metadata);
        return item;
    }

    function taskCard(task) {
        const item = card();
        item.classList.add('task-card');
        const status = node('span', labels[task.state] || task.state, 'task-status');
        status.dataset.state = task.state;
        item.append(node('h3', task.title), status, taskDates(task, false));
        if (task.id === telegramTask?.id) {
            item.setAttribute('aria-current', 'true');
            if (view === 'open' || !Number.isSafeInteger(telegramTask.revision)) {
                item.append(node('span', 'Next in Telegram', 'task-route meta'));
            }
        }
        if (task.recent_reply) item.append(node('p', task.recent_reply, 'task-preview'));
        item.append(row(taskButton(task.state === 'waiting' ? 'Review requests' : 'Open task', task.id), ...controls(task)));
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
            buttons.push(action('Continue', 'select-task', input, `Your next Telegram message continues “${task.title}”.`, true));
        }
        if (Number.isSafeInteger(task.revision)) {
            if ([...stoppable, 'idle', 'blocked'].includes(task.state)) {
                buttons.push(action('Mark complete', 'complete-task', input, 'Task marked complete.', true));
            }
            if (stoppable.includes(task.state)) buttons.push(action('Stop', 'stop-task', input, 'Task stopped.', true));
        }
        return buttons;
    }

    function checklist(steps) {
        const item = card('Task plan');
        const list = node('ol');
        for (const step of steps) {
            const entry = node('li');
            const completed = step.completed_at != null;
            entry.append(node('strong', step.title), document.createTextNode(' · '),
                node('span', completed ? 'Verified' : 'Planned', 'meta'));
            if (step.checkpoint) entry.append(node('p', step.checkpoint));
            list.append(entry);
        }
        item.append(list);
        return item;
    }

    async function detail(id) {
        const loading = node('p', 'Loading task…', 'meta');
        loading.setAttribute('role', 'status');
        const body = node('div', undefined, 'task-detail');
        body.append(loading);
        const back = button('Back to tasks', () => select(null), true);
        back.classList.add('back-link');
        detailView.append(back, body);
        try {
            const data = await api('task-detail', {task: id});
            if (!body.isConnected) return;
            const {task, reply, audit, requests} = data;
            const status = card(task.title, labels[task.state] || task.state);
            status.append(taskDates(task));
            if (task.id === state.active_task) status.append(node('span', 'Current Chief task', 'meta'));
            if (data.executing) status.append(node('p', 'An action is in progress. Refresh to see its result.'));
            else if (data.failure) status.append(node('p', data.failure));
            else {
                const guidance = {
                    completed: 'Reopen this task to continue in Telegram.',
                    idle: task.id === state.active_task ? 'Send a follow-up in Telegram to continue.'
                        : 'Choose Continue to route your next Telegram message here.',
                    waiting: requests.length ? '' : 'Check the request in Telegram.',
                    uncertain: 'Verify the result at its destination, then send /reconcile in Telegram.',
                }[task.state];
                if (guidance) status.append(node('p', guidance));
            }
            const buttons = controls(task, data.executing);
            if (task.id === state.active_task && Number.isSafeInteger(task.revision)) buttons.push(startNew(task));
            if (task.state === 'setup') buttons.push(button('Set up model', openModel, true));
            if (audit) buttons.push(button('View audit', () => {
                const backToTask = () => select(task.id);
                setBackAction(backToTask);
                focusedAudit(detailView, state, {...audit, message: task.title}, backToTask);
                window.scrollTo(0, 0);
                detailView.firstElementChild?.focus({preventScroll: true});
            }, true));
            if (buttons.length) status.append(row(...buttons));
            body.replaceChildren(status);
            if (requests.length) body.append(node('h2', 'Needs you'), ...requests.map(request => review(request)));
            if (data.plan?.length) body.append(checklist(data.plan));
            if (data.last_confirmed) body.append(card('Last confirmed step', data.last_confirmed));
            if (reply) {
                const latest = card('Latest reply');
                latest.append(node('div', reply.text, 'task-reply'));
                if (reply.truncated) latest.append(node('p', 'Open the Telegram conversation for the full reply.', 'meta'));
                body.append(latest);
            }
            if (data.files?.length) body.append(fileShelf(task, data.files, date));
            if (data.docblock_saves) body.append(saveToDocblock(state, task, reply, data.docblock_saves, select));
        } catch (error) {
            if (!body.isConnected) return;
            const item = card(null, error.message);
            if (error.status !== 404) item.append(button('Retry loading task', renderSelection, true));
            body.replaceChildren(item);
        }
    }

    renderSelection();
}
