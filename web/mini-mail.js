import { api, content, refresh, show } from './mini.js';
import { button, card, field, form, node, row } from './mini-ui.js';

const inbox = {name: 'INBOX', automate: false};
const maxAccounts = 10;
const maxFolders = 12;

export function mail(state) {
    const heading = node('h2', 'Email');
    heading.tabIndex = -1;
    const overview = card(null, 'Choose which folders your Chief can handle automatically. For other watched folders, open the Telegram notice with Read more or reply to it to share the email with your Chief. Sending email always needs your approval.');
    const accounts = state.mail_accounts ?? [];
    if (accounts.length < maxAccounts) overview.append(button('Add email account', () => editAccount(state)));
    content.append(heading, overview);

    for (const account of accounts) {
        const item = card(account.label, account.address);
        item.classList.add('mail-card');
        const protocols = [account.imap && 'Receive via IMAP', account.smtp && 'Send via SMTP'].filter(Boolean);
        item.append(node('p', protocols.join(' · '), 'meta'));
        if (account.imap && account.folders.length) {
            const list = node('ul', undefined, 'mail-folder-list');
            for (const folder of account.folders) {
                const entry = node('li');
                entry.append(node('strong', folder.name), node('span', folder.automate ? 'Automatic handling' : 'Notify only', 'meta'));
                list.append(entry);
            }
            item.append(list);
        }
        const edit = button('Edit account', () => editAccount(state, account), true);
        const confirm = button('Remove account', async () => {
            await api('remove-mail-account', {id: account.id});
            await refresh();
            show('Email account removed.');
        });
        const remove = button('Remove account', () => {
            removal.hidden = false;
            actions.hidden = true;
            confirm.focus();
        }, true);
        const removal = node('div');
        removal.hidden = true;
        removal.append(node('p', 'Remove this email account and stop watching its folders?'), row(
            confirm, button('Keep account', () => {
                removal.hidden = true;
                actions.hidden = false;
                remove.focus();
            }, true)
        ));
        const actions = row(edit, remove);
        item.append(actions, removal);
        content.append(item);
    }
}

function editAccount(state, account) {
    const label = field('label', 'Account name', {max: 80, value: account?.label});
    const address = field('address', 'From email', {type: 'email', max: 254, value: account?.address});
    address.querySelector('input').spellcheck = false;
    const folders = watchedFolders(account?.folders);
    const imap = protocol('imap', 'Receive via IMAP', account?.imap, !account || Boolean(account.imap));
    imap.inputs.append(folders.element);
    const smtp = protocol('smtp', 'Send via SMTP', account?.smtp, Boolean(account?.smtp));
    const back = () => {
        content.replaceChildren();
        mail(state);
        content.querySelector('h2').focus();
    };
    const editor = form('Save account', [label, address, imap.element, smtp.element], async (data, element) => {
        const config = {
            label: data.label.trim(),
            address: data.address.trim(),
            imap: imap.config(data),
            smtp: smtp.config(data),
            folders: imap.enabled.checked ? folders.values() : []
        };
        if (!config.imap && !config.smtp) throw new Error('Turn on IMAP, SMTP, or both.');
        for (const input of element.querySelectorAll('input[type=password]')) input.value = '';
        await api('save-mail-account', account ? {id: account.id, config} : {config});
        await refresh();
        show('Email account saved.');
    });
    const save = editor.querySelector('button[type=submit]');
    editor.append(row(save, button('Cancel', back, true)));
    const item = card(account ? 'Edit email account' : 'Add email account');
    item.classList.add('mail-card');
    item.append(editor);
    content.replaceChildren(item);
    label.querySelector('input').focus();
}

function checkbox(text, checked) {
    const label = node('label', undefined, 'mail-check');
    const input = node('input');
    input.type = 'checkbox';
    input.checked = checked;
    label.append(input, document.createTextNode(text));
    return {label, input};
}

function protocol(name, title, config, enabled) {
    const acronym = name.toUpperCase();
    const host = field(name + '-host', acronym + ' host', {max: 253, value: config?.host});
    const username = field(name + '-username', acronym + ' username', {max: 254, value: config?.username});
    const password = field(name + '-password', acronym + ' password', {
        type: 'password', max: 512, required: !config,
        hint: config ? 'Leave blank to keep the saved password. Enter a password when changing the host, port or username.' : undefined
    });
    password.querySelector('input').autocomplete = 'new-password';
    for (const field of [host, username]) {
        const input = field.querySelector('input');
        input.spellcheck = false;
        input.autocapitalize = 'none';
    }
    const security = name === 'smtp' ? field('smtp-port', 'SMTP security', {
        choices: [['465', 'Implicit TLS (465)'], ['587', 'STARTTLS (587)']],
        value: String(config?.port ?? 587)
    }) : node('p', 'IMAP uses TLS on port 993.', 'meta');
    const toggle = checkbox(title, enabled);
    const inputs = node('fieldset', undefined, 'mail-protocol-inputs');
    inputs.setAttribute('aria-label', acronym + ' settings');
    inputs.append(host, security, username, password);
    function sync() {
        inputs.hidden = inputs.disabled = !toggle.input.checked;
    }
    toggle.input.addEventListener('change', sync);
    sync();
    const element = node('section', undefined, 'mail-protocol');
    element.append(toggle.label, inputs);
    return {
        element, inputs, enabled: toggle.input,
        config: data => {
            if (!toggle.input.checked) return null;
            const host = data[name + '-host'].trim().replace(/\.+$/, '').toLowerCase();
            const port = name === 'smtp' ? Number(data['smtp-port']) : 993;
            const username = data[name + '-username'].trim();
            const password = data[name + '-password'];
            if (!password && (host !== config?.host || port !== config?.port || username !== config?.username)) {
                throw new Error(`Enter the ${acronym} password to change the connection.`);
            }
            return {host, port, username, password};
        }
    };
}

function watchedFolders(initial) {
    const element = node('div', undefined, 'mail-watch');
    element.append(node('h3', 'Watched folders'), node('p', 'Enter each folder’s IMAP name. New folders default to Notify only.', 'meta'));
    const list = node('div');
    const folders = [];
    const add = button('Add watched folder', () => addFolder({name: '', automate: false}).focus(), true);
    element.append(list, add);

    function addFolder(folder) {
        const entry = node('div', undefined, 'mail-watch-entry');
        const name = field('folder-name', 'Folder name', {max: 128, value: folder.name});
        const input = name.querySelector('input');
        const automate = checkbox('Let the agent handle new mail', folder.automate);
        const instruction = field('instruction', 'Handling instructions (optional)', {
            type: 'textarea', max: 1500, required: false, value: folder.instruction ?? ''
        });
        const instructions = instruction.querySelector('textarea');
        const sync = () => { instruction.hidden = instructions.disabled = !automate.input.checked; };
        automate.input.addEventListener('change', sync);
        sync();

        const value = () => ({
            name: input.value.trim(),
            automate: automate.input.checked,
            instruction: automate.input.checked ? instructions.value.trim() || null : null
        });
        const remove = button('Remove folder', () => {
            folders.splice(folders.indexOf(item), 1);
            entry.remove();
            add.focus();
            syncButtons();
        }, true);
        const item = {value, remove};
        folders.push(item);
        entry.append(name, automate.label, instruction, remove);
        list.append(entry);
        syncButtons();
        return input;
    }

    function syncButtons() {
        add.hidden = folders.length >= maxFolders;
        for (const folder of folders) folder.remove.hidden = folders.length === 1;
    }

    for (const folder of initial?.length ? initial : [inbox]) addFolder(folder);
    return {element, values: () => folders.map(folder => folder.value())};
}
