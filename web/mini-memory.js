import { action, button, card, field, form, node, row } from './mini-ui.js';
import { api, content, show, telegram } from './mini.js';
import { projectSpaces } from './mini-projects.js';

let selectedScope = 'private';

export function memories(state) {
    const scopes = new Map([
        ['private', ['Chief (private)', 'Available to your Chief.', {scope: 'private'}]],
        ['group', ['Group', 'Available to your Chief and your owned helpers.', {scope: 'group'}]],
    ]);
    for (const helper of state.agents.filter(agent => agent.owner === state.user)) {
        scopes.set('agent:' + helper.id, [
            'Helper · ' + helper.name,
            'Available to this helper when working on your requests.',
            {scope: 'agent', agent: helper.id},
        ]);
    }
    if (!scopes.has(selectedScope)) selectedScope = 'private';

    const date = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeZone: state.time_zone});
    const picker = field('scope', 'Memory scope', {
        choices: Array.from(scopes, ([key, [label]]) => [key, label]),
        value: selectedScope,
    });
    const select = picker.querySelector('select');
    const panel = node('div');
    const settings = card('Memory');
    settings.append(picker, panel);
    content.append(settings, projectSpaces(state));
    if (state.shared_memory_available) {
        const shared = node('div');
        content.append(shared);
        void loadSharedMemories(shared, state);
    }

    function selectScope() {
        selectedScope = select.value;
        const [, description, scope] = scopes.get(selectedScope);
        panel.replaceChildren(node('p', description), memoryScope(scope, date));
        show('');
    }

    select.addEventListener('change', selectScope);
    selectScope();
}

async function loadSharedMemories(container, state) {
    try {
        const {pairs} = await api('shared-memory-pairs');
        if (pairs.length === 0) return;
        container.replaceChildren(node('h2', 'Shared memory'));
        for (const pair of pairs) {
            const name = pair.peer_name || (state.operator ? `Guest ${pair.peer}` : 'Bot operator');
            const item = card(name, 'Only notes either of you publishes here are shared. Each person’s chat stays private. Published notes can be sent to the model providers you use in this space.');
            const data = {pair: pair.id};
            if (!pair.joined && pair.peer_joined) {
                item.append(action('Join shared memory', 'join-shared-memory', data, 'You joined the shared memory space.'));
            }
            if (pair.joined) {
                if (!pair.ready) item.append(node('p', 'The other person must join and have active access before you can use the space.', 'meta'));
                const leave = button('Leave shared memory', async () => {
                    if (!window.confirm('Leave this space? Existing shared-memory conversations will close for both people.')) return;
                    await api('leave-shared-memory', data);
                    await loadSharedMemories(container, state);
                    show('You left the shared memory space.');
                }, true);
                if (pair.ready) {
                    item.append(row(button('Start shared chat', async () => {
                        await api('open-shared-memory', data);
                        show('Send your next message in Telegram.');
                        telegram?.close?.();
                    }), leave));
                    const notes = node('div');
                    item.append(form('Publish shared note', [field('body', 'Shared note', {type: 'textarea', max: 1200})], async (values, editor) => {
                        await api('publish-shared-memory', {...data, body: values.body});
                        editor.reset();
                        await loadSharedNotes(notes, data);
                        show('Note published.');
                    }), notes);
                    void loadSharedNotes(notes, data);
                } else item.append(leave);
            }
            if (!pair.joined && !pair.peer_joined) item.append(node('p', 'This space is closed.', 'meta'));
            container.append(item);
        }
    } catch (error) {
        container.replaceChildren(button('Retry loading shared memory', () => loadSharedMemories(container, state), true));
    }
}

async function loadSharedNotes(container, data) {
    try {
        const {notes} = await api('shared-memory-notes', data);
        container.replaceChildren(...notes.map(note => {
            const item = card();
            item.append(node('p', note.body, 'memory-note'));
            if (note.own) item.append(button('Remove shared note', async () => {
                if (!window.confirm('Remove this note? Existing shared-memory conversations will close for both people.')) return;
                await api('remove-shared-memory', {...data, id: note.id});
                await loadSharedNotes(container, data);
                show('Shared note removed.');
            }, true));
            return item;
        }));
        if (notes.length === 0) container.append(node('p', 'No shared notes yet.', 'meta'));
    } catch (error) {
        container.replaceChildren(node('p', error.message, 'meta'));
    }
}

function memoryScope(scope, dates) {
    const panel = node('div');
    const entries = node('div');
    const status = node('p', '', 'meta');
    status.setAttribute('role', 'status');
    const more = button('Load older memories', () => load(nextBefore), true);
    let nextBefore = null;
    let generation = 0;

    const editor = form('Save memory', [field('body', 'Memory', {type: 'textarea', max: 1200})], async (data, element) => {
        await api('save-memory', {...scope, body: data.body});
        element.reset();
        show('Memory saved.');
        await load();
    });
    panel.append(editor, entries, status, more);
    void load();
    return panel;

    async function load(before = null) {
        const request = ++generation;
        status.hidden = false;
        status.textContent = 'Loading memories…';
        more.hidden = true;
        try {
            const page = await api('list-memories', before === null ? scope : {...scope, before});
            if (request !== generation) return;
            const records = page.entries.map(entry => memoryEntry(entry, dates, remove));
            if (before === null) entries.replaceChildren(...records);
            else entries.append(...records);
            nextBefore = page.next_before ?? null;
            status.textContent = 'No memories saved here yet.';
            status.hidden = entries.childElementCount > 0;
            more.hidden = nextBefore === null;
            more.textContent = 'Load older memories';
        } catch (error) {
            if (request !== generation) return;
            status.textContent = error.message;
            nextBefore = before;
            more.textContent = 'Retry loading memories';
            more.hidden = false;
        }
    }

    async function remove(id) {
        await api('delete-memory', {id});
        show('Memory deleted.');
        await load();
    }
}

function memoryEntry(entry, dates, remove) {
    const item = card();
    item.append(node('p', entry.body, 'memory-note'));
    if (entry.created_at > 0) {
        item.append(node('p', dates.format(new Date(entry.created_at * 1000)), 'meta'));
    }
    item.append(button('Delete memory', () => remove(entry.id), true));
    return item;
}
