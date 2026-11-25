import { api } from './mini.js';
import { button, card, node, row } from './mini-ui.js';

const delivery = {
    queued: 'Queued for Telegram', sending: 'Sending to Telegram', sent: 'Sent to Telegram',
    failed: 'Telegram delivery failed', uncertain: 'Telegram delivery unconfirmed', skipped: 'Telegram delivery skipped',
};

export function fileShelf(task, files, date) {
    const section = node('section');
    section.append(node('h2', 'Files'));
    for (const file of files) {
        const description = file.kind === 'upload' ? 'Telegram upload' : `Generated file · Version ${file.version}`;
        const item = card(file.name, description);
        item.classList.add('task-card');
        if (delivery[file.delivery]) item.append(node('span', delivery[file.delivery], 'meta'));
        if (file.expires_at != null) item.append(node('span', `Available until ${date(file.expires_at)}`, 'meta'));
        item.append(row(button('Download', async () => {
            const result = await api('download-task-file', {task: task.id, kind: file.kind, id: file.id});
            download(result.name, Uint8Array.from(atob(result.content), byte => byte.charCodeAt(0)));
        }, true)));
        section.append(item);
    }
    return section;
}

function download(name, bytes) {
    const safeName = name.replace(/[^\p{L}\p{N}._ -]/gu, '_').replace(/^\.+/, '').trim();
    const url = URL.createObjectURL(new Blob([bytes], {type: 'application/octet-stream'}));
    const link = node('a');
    link.href = url;
    link.download = (safeName || 'task-file').slice(0, 180);
    link.hidden = true;
    document.body.append(link);
    try { link.click(); }
    finally {
        link.remove();
        setTimeout(() => URL.revokeObjectURL(url), 30_000);
    }
}
