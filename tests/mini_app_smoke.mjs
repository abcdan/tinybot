import assert from 'node:assert/strict';
import {createHmac} from 'node:crypto';
import {readFileSync} from 'node:fs';

const bot = readFileSync(process.env.TELEGRAM_BOT_TOKEN_FILE, 'utf8').trim();
const origin = process.env.TINYBOT_ORIGIN;
const user = process.env.TELEGRAM_OPERATOR_ID;
const fields = new URLSearchParams({auth_date: String(Math.floor(Date.now() / 1000)),
    user: JSON.stringify({id: Number(user), first_name: 'Smoke'})});
fields.sort();
const key = createHmac('sha256', 'WebAppData').update(bot).digest();
const check = [...fields].map(([name, value]) => name + '=' + value).join('\n');
fields.set('hash', createHmac('sha256', key).update(check).digest('hex'));

async function api(action, data = {}, init = fields.toString(), requestOrigin = origin) {
    const response = await fetch('http://127.0.0.1:8080/api/mini', {
        method: 'POST',
        headers: {'Content-Type': 'application/json', Origin: requestOrigin},
        body: JSON.stringify({init, action, data})
    });
    return {status: response.status, data: await response.json()};
}

assert.equal((await api('status', {}, 'invalid')).status, 403);
assert.equal((await api('status', {}, fields.toString(), 'https://foreign.example')).status, 403);
const initial = await api('status');
assert.equal(initial.data.user, user);
const agent = initial.data.agents.find(agent => agent.name === 'Smoke helper');
assert.equal((await api('save-agent', {agent: agent?.id, name: 'Smoke helper', instructions: 'Ask for a quarter.', tools: ['request_input']})).status, 200);
assert.equal((await api('time-zone', {zone: 'Europe/Amsterdam'})).status, 200);
const found = await api('search-mcp', {name: 'stdio://fixture'});
assert.equal(found.data.candidates[0]?.transport, 'stdio');
const configured = await api('add-mcp', {...found.data.candidates[0], confirmed: true});
assert.equal(configured.status, 200, JSON.stringify(configured.data));
const connected = await api('mcp-public', {id: configured.data.id});
assert.equal(connected.status, 200, JSON.stringify(connected.data));
const status = await api('status');
assert.equal(status.data.time_zone, 'Europe/Amsterdam');
assert.equal(status.data.connectors[0]?.auth, 'public');
assert.equal(status.data.agents[0]?.name, 'Smoke helper');
console.log('Mini App passed signature, origin, saved agent, time zone, and live stdio connector setup checks.');
