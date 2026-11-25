import { api, content, refresh, show } from './mini.js';
import { action, button, card, field, form, node, row } from './mini-ui.js';

const number = new Intl.NumberFormat();
const money = new Intl.NumberFormat(undefined, {style: 'currency', currency: 'USD', maximumFractionDigits: 6});

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
    const pairs = [
        ['Last active day', guest.active_day == null ? 'No recent activity' : dates.format(new Date(guest.active_day * 1000))],
        ['Runs started', number.format(guest.runs_started)],
        ['Runs completed', number.format(guest.runs_completed)],
        ['Reported tokens', guest.tokens == null ? 'Not reported' : number.format(guest.tokens)],
        ['Reported cost', guest.cost_usd == null ? 'Not reported' : money.format(guest.cost_usd)],
    ];
    if (guest.model_calls > 0) {
        pairs.push(['Usage coverage', usageCoverage(guest)]);
    }
    if (guest.first_result != null) {
        pairs.push(['First result', guest.first_result ? 'Delivered' : 'Pending'],
            ['Active days', number.format(guest.active_days)],
            ['Results delivered', number.format(guest.delivered_runs)],
            ['Tasks checked off', number.format(guest.checked_off_tasks)]);
    }
    for (const [label, value] of pairs) totals.append(node('dt', label, 'meta'), node('dd', value));
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

function usageCoverage(usage) {
    const calls = number.format(usage.model_calls);
    return `Cost for ${number.format(usage.cost_reported_calls)} of ${calls} calls, tokens for ${number.format(usage.token_reported_calls)} of ${calls}.`;
}

export function accountInsights() {
    const panel = node('details', undefined, 'card');
    const body = node('div');
    panel.append(node('summary', 'Setup and spend alerts'), body);
    let loaded = false;
    panel.addEventListener('toggle', () => {
        if (panel.open && !loaded) void load();
    });
    return panel;

    async function load() {
        body.replaceChildren(node('p', 'Loading your usage…', 'meta'));
        try {
            const {guest, alert} = await api('my-usage');
            loaded = true;
            body.replaceChildren();
            if (guest) {
                const stages = node('ol');
                stages.append(node('li', 'Invitation accepted'),
                    node('li', guest.model_connected ? 'Model connected' : 'Connect your model in Settings'),
                    node('li', guest.first_result_at_ms != null ? 'First result delivered' : 'First result pending'));
                body.append(node('h3', 'Getting started'), stages);
            }
            const amount = alert.spent_microusd == null ? 'Not reported' : money.format(alert.spent_microusd / 1_000_000);
            const spend = node('p', `Reported this week: ${amount}`);
            const coverage = node('p', `Cost was reported for ${number.format(alert.cost_reported_calls)} of ${number.format(alert.model_calls)} calls.`, 'meta');
            const limit = field('amount', 'Weekly alert amount (USD)', {
                max: 8, value: alert.weekly_cents == null ? '' : (alert.weekly_cents / 100).toFixed(2),
                hint: 'Weeks start Monday in UTC. TinyBot sends one alert when reported spend reaches this amount.',
            });
            limit.querySelector('input').inputMode = 'decimal';
            body.append(node('h3', 'Weekly spend alert'), spend, coverage,
                form('Save spend alert', [limit], async ({amount}) => {
                    const text = amount.trim().replace(',', '.');
                    const amountInCents = Math.round(Number(text) * 100);
                    if (!/^[0-9]+(?:\.[0-9]{1,2})?$/.test(text) || amountInCents < 1 || amountInCents > 1_000_000) {
                        throw new Error('Enter an amount between $0.01 and $10,000.');
                    }
                    await api('save-usage-alert', {weekly_cents: amountInCents});
                    show('Spend alert saved.');
                    await load();
                }));
            if (alert.weekly_cents != null) {
                body.append(button('Turn off spend alert', async () => {
                    await api('save-usage-alert', {weekly_cents: null});
                    show('Spend alert turned off.');
                    await load();
                }, true));
            }
        } catch (error) {
            loaded = false;
            body.replaceChildren(node('p', error.message, 'meta'), button('Retry loading your usage', load, true));
        }
    }
}
