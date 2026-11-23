import { button, card, field, form, node } from './mini-ui.js';
import { api, content, show } from './mini.js';

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
    content.append(settings);

    function selectScope() {
        selectedScope = select.value;
        const [, description, scope] = scopes.get(selectedScope);
        panel.replaceChildren(node('p', description), memoryScope(scope, date));
        show('');
    }

    select.addEventListener('change', selectScope);
    selectScope();
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
    const date = dates.format(new Date(entry.created_at * 1000));
    item.append(node('p', entry.body, 'memory-note'), node('p', date, 'meta'),
        button('Delete memory', () => remove(entry.id), true));
    return item;
}
