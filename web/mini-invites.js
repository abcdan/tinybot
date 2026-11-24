import { api, content, refresh, show } from './mini.js';
import { action, button, card, node, row } from './mini-ui.js';

export function invites(state) {
    const setup = card('Invite someone', 'An invitation works once and expires after 24 hours. Each guest sets up their own model and approves access before their account is created.');
    const code = node('pre');
    const copy = button('Copy invitation', async () => {
        await navigator.clipboard.writeText(code.textContent);
        show('Invitation copied. Ask your guest to send it to TinyBot.');
    }, true);
    code.hidden = copy.hidden = true;
    setup.append(button('Create invitation', async () => {
        const invite = await api('create-invite');
        code.textContent = invite.command;
        code.hidden = copy.hidden = false;
        await load();
    }), code, copy);
    const roster = node('div');
    content.append(setup, roster);
    void load();

    async function load() {
        roster.replaceChildren(node('p', 'Loading invitations…', 'meta'));
        try {
            const {pending, guests} = await api('list-invites');
            const expiry = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeStyle: 'short', timeZone: state.time_zone});
            const day = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeZone: 'UTC'});
            const pendingList = pending.map(invite => {
                const item = card('Unused invitation', `Expires ${expiry.format(new Date(invite.expires * 1000))}`);
                item.append(button('Revoke invitation', async () => {
                    await api('revoke-invite', {id: invite.id});
                    code.hidden = copy.hidden = true;
                    show('Invitation revoked.');
                    await load();
                }, true));
                return item;
            });
            const guestList = guests.map(guest => guestCard(guest, day));
            roster.replaceChildren(
                node('h2', 'Pending invitations'), ...(pendingList.length ? pendingList : [card(null, 'No pending invitations.')]),
                node('h2', 'Guests'), node('p', 'Activity covers the past seven days. Costs are provider-reported estimates.', 'meta'),
                ...(guestList.length ? guestList : [card(null, 'No guests have joined yet.')]),
            );
        } catch (error) {
            roster.replaceChildren(card(null, error.message), button('Retry loading invitations', load, true));
        }
    }
}

function guestCard(guest, dates) {
    const name = guest.display_name || `Guest ${guest.id}`;
    const item = card(name, guest.suspended ? 'Suspended' : guest.setup_complete ? 'Model connected' : 'Setup pending');
    if (guest.display_name) item.append(node('p', `Telegram ID ${guest.id}`, 'meta'));
    item.append(node('p', guest.accepted_at ? `Accepted ${dates.format(new Date(guest.accepted_at * 1000))}` : 'Existing account', 'meta'));
    const totals = node('dl');
    const number = new Intl.NumberFormat();
    const money = new Intl.NumberFormat(undefined, {style: 'currency', currency: 'USD', maximumFractionDigits: 6});
    for (const [label, value] of [
        ['Last active day', guest.active_day == null ? 'No recent activity' : dates.format(new Date(guest.active_day * 1000))],
        ['Runs started', number.format(guest.runs_started)],
        ['Runs completed', number.format(guest.runs_completed)],
        ['Reported tokens', guest.tokens == null ? 'Not reported' : number.format(guest.tokens)],
        ['Reported cost', guest.cost_usd == null ? 'Not reported' : money.format(guest.cost_usd)],
    ]) totals.append(node('dt', label, 'meta'), node('dd', value));
    const visibility = button(guest.suspended ? 'Restore guest' : 'Suspend guest', async () => {
        if (!guest.suspended && !window.confirm(`Suspend ${name}? Their active work will stop.`)) return;
        await api(guest.suspended ? 'restore-guest' : 'suspend-guest', {guest: guest.id});
        await refresh();
        show(guest.suspended ? 'Guest access restored.' : 'Guest suspended.');
    }, true);
    if (guest.accepted_at) item.append(totals);
    else item.append(node('p', 'Usage is unavailable for accounts created before invitation consent.', 'meta'));
    item.append(guest.suspended ? row(visibility) : row(
        action('Offer shared memory', 'invite-shared-memory', {guest: guest.id}, 'Shared memory is available when the guest joins.'),
        visibility,
    ));
    return item;
}
