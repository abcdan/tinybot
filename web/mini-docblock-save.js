import { api, refresh, telegram } from './mini.js';
import { button, card, field, form, node, row } from './mini-ui.js';

let openSource;
const saveLabels = {
    review: 'Ready for your review', queued: 'Approved and queued', saving: 'Saving to Docblock',
    saved: 'Saved in Docblock', proposed: 'Awaiting review in Docblock', denied: 'Declined',
    failed: 'Save failed', expired: 'Review expired', superseded: 'Review replaced',
    uncertain: 'Check the note in Docblock',
};

function docblockLink(raw) {
    const url = new URL(raw);
    if (url.origin !== 'https://docblock.dev' || url.username || url.password) {
        throw new Error('Docblock returned an invalid link.');
    }
    return url;
}

export function saveToDocblock(state, task, reply, saves, select) {
    const section = card('Docblock');
    for (const save of saves) {
        const item = node('div');
        item.append(node('p', saveLabels[save.status] || save.status), node('code', save.path));
        if (save.status === 'review') {
            item.append(row(button('Review note', () => select(save.approval_task), true)));
        }
        if (save.url) item.append(row(button(save.status === 'proposed' ? 'Open Docblock review' : 'Open note',
            () => telegram.openLink(docblockLink(save.url).href, {try_instant_view: false}), true)));
        section.append(item);
    }

    const source = `${task.id}/${reply.event}`;
    const wizard = node('div');
    const start = button('Save to Docblock', async () => {
        openSource = source;
        await destination(wizard, state, task, reply, select);
    }, true);
    section.append(row(start), wizard);
    if (openSource === source) start.click();
    return section;
}

async function destination(wizard, state, task, reply, select) {
    const access = Object.entries(state.docblock_access || {});
    const connection = access.find(([, scope]) => ['write', 'propose'].includes(scope));
    if (!connection) {
        const id = access[0]?.[0];
        const signIn = button('Sign in to Docblock', async () => {
            const {consent_url} = await api('docblock-oauth', id ? {id} : {});
            const url = docblockLink(consent_url);
            if (url.pathname !== '/oauth/authorize') throw new Error('Docblock returned an invalid sign-in link.');
            telegram.openLink(url.href, {try_instant_view: false});
        });
        wizard.replaceChildren(node('p', 'Grant TinyBot access to the workspace where you want to save the note.'),
            row(signIn, button('Check connection', () => refresh(), true)));
        return;
    }

    const [id, scope] = connection;
    const result = await api('docblock-workspaces', {id});
    const workspaces = result.workspaces.filter(({role}) => ['owner', 'editor'].includes(role));
    if (!workspaces.length) {
        wizard.replaceChildren(node('p', 'This connection has no workspaces you can edit.'));
        return;
    }
    const workspace = field('workspace', 'Docblock workspace', {
        choices: workspaces.map(({id, name}) => [id, name])
    });
    const folders = field('folder', 'Docblock folder', {choices: [], required: false});
    const title = field('title', 'Note title', {max: 160, value: task.title.slice(0, 120)});
    const status = node('p', undefined, 'meta');
    status.setAttribute('role', 'status');
    const input = form('Review note', [workspace, folders, title], async values => {
        const save = await api('save-result-to-docblock', {
            ...values, id, task: task.id, revision: task.revision, event: reply.event
        });
        openSource = undefined;
        if (save.status === 'review') select(save.approval_task);
        else await refresh();
    });
    const workspaceControl = workspace.querySelector('select');
    const folderControl = folders.querySelector('select');
    const submit = input.querySelector('button');
    let generation = 0;
    async function loadFolders() {
        const sequence = ++generation;
        folderControl.disabled = true;
        submit.disabled = true;
        status.textContent = 'Loading Docblock folders…';
        try {
            const {folders} = await api('docblock-folders', {id, workspace: workspaceControl.value});
            if (generation !== sequence) return;
            folderControl.replaceChildren(new Option('Workspace root', ''));
            for (const folder of folders) folderControl.add(new Option(folder, folder));
            folderControl.disabled = false;
            submit.disabled = false;
            status.textContent = '';
        } catch (error) {
            if (generation === sequence) status.replaceChildren(
                node('span', error.message), button('Retry folders', loadFolders, true)
            );
        }
    }
    workspaceControl.addEventListener('change', loadFolders);
    wizard.replaceChildren(node('p', scope === 'propose' ? 'Docblock will review the proposal after you approve it.' :
        'The approved note will use this task’s latest reply.'), input, status);
    await loadFolders();
}
