import { api, content } from './mini.js';
import { button, card, node, row } from './mini-ui.js';

const source = {settings: 'Saved in the Mini App', approval: 'Approved settings change', restore: 'Approved restore'};

export function settingsHistory(state, review) {
    const panel = node('div');
    content.append(node('h2', 'Settings history'), panel);
    const date = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeStyle: 'short', timeZone: state.time_zone});

    function entry(change) {
        const section = card(source[change.source], date.format(new Date(change.created * 1000)));
        for (const value of change.changes) {
            const values = node('dl', undefined, 'settings-changes');
            values.append(node('dt', 'Before'), node('dd', value.before), node('dt', 'After'), node('dd', value.after));
            section.append(node('h4', value.label), values);
        }
        if (change.can_restore) section.append(row(button('Review restore', async () => {
            const request = await api('restore-settings', {id: change.id});
            if (!panel.isConnected) return;
            panel.replaceChildren(button('Back to settings history', start, true), review(request));
            panel.querySelector('button').focus();
        }, true)));
        return section;
    }

    async function page(before) {
        const result = await api('settings-history', before ? {before} : {});
        if (!panel.isConnected) return;
        if (!before) panel.replaceChildren();
        panel.append(...result.items.map(entry));
        if (!before && !result.items.length) panel.append(card(null, 'No personal settings changes yet.'));
        if (result.next_before != null) {
            const more = button('Load older changes', async () => {
                await page(result.next_before);
                more.remove();
            }, true);
            panel.append(more);
        }
    }

    function start() {
        const loading = node('p', 'Loading settings history…', 'meta');
        loading.setAttribute('role', 'status');
        panel.replaceChildren(loading);
        page().catch(error => {
            if (panel.isConnected) panel.replaceChildren(card(null, error.message), button('Retry history', start, true));
        });
    }

    start();
}
