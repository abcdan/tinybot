import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {test} from 'node:test';
import {chromium} from 'playwright';

async function mockMiniApp(context, respond, bridge) {
    await context.route('https://telegram.org/**', route => route.fulfill({
        contentType: 'application/javascript',
        body: bridge ?? 'window.Telegram = {WebApp: {initData: "fixture", ready() {}, close() { window.miniAppClosed = true; }}};'
    }));
    await context.route('https://tinybot.test/**', async route => {
        const path = new URL(route.request().url()).pathname;
        if (path === '/api/mini') {
            const request = route.request().postDataJSON();
            assert.equal(request.init, 'fixture');
            return route.fulfill({json: respond(request)});
        }
        const file = path === '/' || path === '/mini' ? 'mini.html' : path.slice(1);
        if (!/^[a-z0-9-]+\.(html|css|js)$/.test(file)) return route.abort();
        const type = file.endsWith('.css') ? 'text/css' : file.endsWith('.html') ? 'text/html' : 'application/javascript';
        return route.fulfill({contentType: type, body: await readFile('/opt/tinybot/web/' + file, 'utf8')});
    });
}

test('schedules edit instructions, pause and run a saved routine', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 640}});
        const page = await context.newPage();
        const schedule = {
            id: 'fixture-routine', title: 'Weekly review', state: 'active', revision: 0,
            time_zone: 'Europe/Amsterdam', cron: '0 9 * * 1',
        };
        const state = {
            user: '1', time_zone: 'Europe/Amsterdam', provider: 'openrouter',
            requests: [], tasks: [], agents: [], schedules: [schedule],
        };
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            assert.equal(data.id, schedule.id);
            if (action === 'schedule-details') {
                return {...schedule, instructions: 'Draft the review.', steps: [{instruction: 'Collect updates.'}]};
            }
            if (action === 'edit-schedule') {
                assert.equal(data.revision, 0);
                assert.deepEqual(data.routine, {
                    title: 'Weekly review', input: 'Prepare a concise review.',
                    cron: '0 9 * * 1', steps: [{instruction: 'Collect updates.', agent: null}],
                });
                schedule.revision++;
                return {ok: true};
            }
            if (action === 'pause-schedule') {
                assert.equal(data.revision, 1);
                schedule.state = 'paused';
                schedule.revision++;
                return {ok: true};
            }
            if (action === 'run-schedule') {
                assert.equal(data.revision, 2);
                assert.match(data.request_id, /^[a-f0-9-]{36}$/);
                return {task: 'fixture-run'};
            }
            if (action === 'schedule-history') {
                return {runs: [{task: 'fixture-run', title: 'Weekly review', state: 'idle', trigger: 'manual', started_at: 1795526400}],
                    occurrences: [{due_at: 1795526400, outcome: 'skipped_overlap'}]};
            }
            if (action === 'schedule-policy') {
                assert.equal(data.revision, 2);
                assert.deepEqual(data.policy, {catch_up: 'skip', quiet_start: 1320, quiet_end: 420});
                schedule.revision++;
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        });
        await page.goto('https://tinybot.test/');
        await page.getByRole('button', {name: 'Schedules'}).click();
        await page.getByText('Edit schedule', {exact: true}).click();
        await page.getByLabel('Instructions', {exact: true}).fill('Prepare a concise review.');
        await page.getByRole('button', {name: 'Save changes'}).click();
        await page.getByText('Schedule updated.').waitFor();
        await page.getByRole('button', {name: 'Pause', exact: true}).click();
        await page.getByRole('button', {name: 'Resume', exact: true}).waitFor();
        await page.getByRole('button', {name: 'Run now', exact: true}).click();
        await page.getByText('Run queued.').waitFor();
        await page.getByText('Run history', {exact: true}).click();
        await page.getByText('Manual · Finished', {exact: false}).waitFor();
        await page.getByText('Skipped · previous run still open', {exact: true}).waitFor();
        await page.getByText('Timing policy', {exact: true}).click();
        await page.getByLabel('After downtime').selectOption('skip');
        await page.getByLabel('Quiet hours start').fill('22:00');
        await page.getByLabel('Quiet hours end').fill('07:00');
        await page.getByRole('button', {name: 'Save timing policy'}).click();
        await page.getByText('Timing policy saved.').waitFor();
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
    } finally {
        await browser.close();
    }
});

test('task search stops the specific returned revision', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 640}});
        const page = await context.newPage();
        const state = {user: '1', provider: 'openrouter', requests: [], tasks: []};
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'search-tasks') {
                assert.deepEqual(data, {query: 'release summary', offset: 0});
                return {items: [{id:'fixture-task', title:'Release notes', state:'running', revision:3}], next_offset:null};
            }
            if (action === 'stop-task') {
                assert.deepEqual(data, {task: 'fixture-task', revision: 3});
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        });
        await page.goto('https://tinybot.test/');
        await page.getByLabel('Search retained messages and tasks').fill('release summary');
        await page.getByRole('button', {name:'Search tasks'}).click();
        const task = page.getByRole('heading', {name:'Release notes'}).locator('..');
        await task.getByRole('button', {name:'Stop',exact:true}).click();
        await page.getByText('Task stopped.', {exact:true}).waitFor();
    } finally {
        await browser.close();
    }
});

test('completing a task moves it to paginated history and reopening selects it again', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 640}});
        const page = await context.newPage();
        const current = {id: 'current', title: 'Send the receipt', state: 'running', revision: 7};
        const followUp = {id: 'follow-up', title: 'Discuss next month', state: 'idle', revision: 3};
        const archived = {id: 'archived', title: 'Finish the older plan', state: 'completed', revision: 2};
        const state = {user: '1', provider: 'openrouter', requests: [], tasks: [current, followUp], active_task: current.id};
        let historyCalls = 0;
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'complete-task') {
                assert.deepEqual(data, {task: current.id, revision: 7});
                current.state = 'completed';
                current.revision++;
                state.active_task = null;
                return {ok: true};
            }
            if (action === 'search-tasks' && data.completed_only) {
                historyCalls++;
                assert.equal(data.query, '');
                if (current.state !== 'completed' || data.offset === 1) {
                    return {items: [archived], next_offset: null};
                }
                assert.equal(data.offset, 0);
                return {items: [current], next_offset: 1};
            }
            if (action === 'search-tasks') {
                assert.deepEqual(data, {query: 'older plan', offset: 0});
                return {items: [archived], next_offset: null};
            }
            if (action === 'select-task') {
                assert.deepEqual(data, {task: current.id});
                current.state = 'idle';
                state.active_task = current.id;
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        });
        await page.goto('https://tinybot.test/mini');
        const task = page.getByRole('heading', {name: current.title}).locator('..');
        await task.getByRole('button', {name: 'Mark complete', exact: true}).click();
        await page.getByText('Task marked complete.', {exact: true}).waitFor();
        await task.waitFor({state: 'hidden'});
        assert.equal(historyCalls, 0);
        await page.getByRole('heading', {name: followUp.title}).locator('..')
            .getByText('Ready for a follow-up', {exact: true}).waitFor();

        await page.getByText('Completed tasks', {exact: true}).click();
        await task.getByText('Completed', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Load more tasks', exact: true}).click();
        await page.getByRole('heading', {name: archived.title}).waitFor();
        await task.getByRole('button', {name: 'Reopen', exact: true}).click();
        await task.getByText('Current Chief task', {exact: true}).waitFor();
        await page.getByLabel('Search retained messages and tasks').fill('older plan');
        await page.getByRole('button', {name: 'Search tasks', exact: true}).click();
        await page.getByRole('heading', {name: archived.title}).locator('..')
            .getByRole('button', {name: 'Reopen', exact: true}).waitFor();
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
    } finally {
        await browser.close();
    }
});

test('first-run setup checks one key, searches tool models and opens the named Chief', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 620}, reducedMotion: 'reduce'});
        const page = await context.newPage();
        const profile = {user: '1', onboarding_required: true, requests: [], tasks: []};
        const input = {display_name: 'Lina', chief_name: 'Atlas', key: 'sk-or-fixture'};
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return profile;
            if (action === 'onboarding-models') {
                assert.deepEqual(data, {key: input.key});
                return {models: [
                    {id: 'fixture/alpha', name: 'Alpha'},
                    {id: 'fixture/chosen', name: 'Another model'},
                ]};
            }
            if (action === 'complete-onboarding') {
                assert.deepEqual(data, {...input, model: 'fixture/chosen'});
                Object.assign(profile, {display_name: input.display_name, chief_name: input.chief_name, provider: 'openrouter', onboarding_required: false});
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        }, `window.Telegram = {WebApp: {
            initData: 'fixture', ready() {}, openLink(url) { window.openedLink = url; },
        }};`);
        await page.goto('https://tinybot.test/');
        await page.getByRole('heading', {name: 'Set up TinyBot'}).waitFor();
        assert.equal(await page.getByRole('navigation', {name: 'TinyBot settings'}).count(), 0);
        assert.equal(await page.locator('.onboarding-screen').evaluate(element => getComputedStyle(element).animationName), 'none');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
        await page.getByRole('link', {name: 'Get an OpenRouter API key'}).click();
        assert.equal(await page.evaluate(() => window.openedLink), 'https://openrouter.ai/settings/keys');
        await page.getByLabel('Your name', {exact: true}).fill(input.display_name);
        await page.getByLabel('Agent name', {exact: true}).fill(input.chief_name);
        await page.getByLabel('OpenRouter API key', {exact: true}).fill(input.key);
        await page.getByRole('button', {name: 'Continue', exact: true}).click();
        const modelHeading = page.getByRole('heading', {name: 'Choose a model'});
        await modelHeading.waitFor();
        assert.equal(await modelHeading.evaluate(heading => heading === document.activeElement), true);
        assert.equal(await page.locator('input[type=password]').count(), 0);
        await page.getByRole('searchbox', {name: 'Search models'}).fill('nothing');
        await page.getByText('No models match your search.').waitFor();
        assert.equal(await page.getByRole('button', {name: 'Finish setup'}).isDisabled(), true);
        await page.getByRole('searchbox', {name: 'Search models'}).fill('another');
        await page.getByRole('combobox', {name: 'Task model'}).selectOption('fixture/chosen');
        await page.getByRole('button', {name: 'Finish setup'}).click();
        await page.getByRole('heading', {name: 'Requests'}).waitFor();
        assert.equal(await page.locator('#chief-label').textContent(), input.chief_name);
        assert.equal(await page.locator('input[type=password]').count(), 0);
        assert.deepEqual(await page.evaluate(() => [localStorage.length, sessionStorage.length]), [0, 0]);
    } finally {
        await browser.close();
    }
});

test('cancelled setup clears the key even while catalog verification is pending', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        await mockMiniApp(context, ({action}) => {
            assert.equal(action, 'status');
            return {user: '1', onboarding_required: true, requests: [], tasks: []};
        });
        let release;
        const gated = new Promise(resolve => { release = resolve; });
        await page.route('**/api/mini', async route => {
            if (route.request().postDataJSON().action !== 'onboarding-models') return route.fallback();
            await gated;
            return route.fulfill({json: {models: [{id: 'fixture/alpha', name: 'Alpha'}]}});
        });
        await page.goto('https://tinybot.test/');
        await page.getByLabel('Your name', {exact: true}).fill('Lina');
        await page.getByLabel('Agent name', {exact: true}).fill('Atlas');
        await page.getByLabel('OpenRouter API key', {exact: true}).fill('sk-or-fixture');
        await page.getByRole('button', {name: 'Continue', exact: true}).click();
        await page.getByText('Checking OpenRouter…', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Cancel setup', exact: true}).click();
        assert.equal(await page.evaluate(() => window.miniAppClosed), true);
        assert.equal(await page.locator('input[type=password]').count(), 0);
        const response = page.waitForResponse(response => response.request().postDataJSON()?.action === 'onboarding-models');
        release();
        await response;
        await page.getByRole('button', {name: 'Start setup', exact: true}).click();
        const key = page.getByLabel('OpenRouter API key', {exact: true});
        await key.waitFor();
        assert.equal(await key.inputValue(), '');
        assert.equal(await page.getByRole('heading', {name: 'Choose a model'}).count(), 0);
    } finally {
        await browser.close();
    }
});

test('MCP approvals show a plain explanation and can be revoked for the exact connector tool', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 360, height: 720}});
        const page = await context.newPage();
        const scope = {connector: 'notes', tool: 'search_and_read'};
        const explanation = 'Search project notes for the latest release.';
        const state = {
            user: '1', provider: 'codex', tasks: [], mcp_approvals: [],
            connectors: [{id: 'notes', name: 'Notes', endpoint: 'https://notes.example/mcp', auth: 'oauth'}],
            requests: [{id: 'request-1', kind: 'approve', content: {
                operation: 'MCP tool search_and_read', destination: 'https://notes.example',
                explanation, mcp: true, rememberable: true
            }}],
        };
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'always-approve-mcp') {
                assert.deepEqual(data, {id: 'request-1'});
                state.requests = [];
                state.mcp_approvals = [scope];
                return {ok: true};
            }
            if (action === 'revoke-mcp-approval') {
                assert.deepEqual(data, scope);
                state.mcp_approvals = [];
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        });
        await page.goto('https://tinybot.test/');
        await page.getByText(explanation, {exact: true}).waitFor();
        assert.equal(await page.locator('#view pre').count(), 0);
        assert.equal(await page.getByRole('button', {name: 'Approve once', exact: true}).count(), 1);
        assert.equal(await page.getByRole('button', {name: 'Deny', exact: true}).count(), 1);
        await page.getByRole('button', {name: 'Always approve', exact: true}).click();
        await page.getByText('Approval saved for this tool.', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Connections', exact: true}).click();
        await page.getByRole('heading', {name: 'Always approved tools', exact: true}).waitFor();
        await page.getByRole('button', {name: 'Revoke approval for search_and_read', exact: true}).click();
        await page.getByText('Approval revoked.', {exact: true}).waitFor();
        assert.equal(await page.getByRole('heading', {name: 'Always approved tools', exact: true}).count(), 0);
    } finally {
        await browser.close();
    }
});

test('a paired Codex model becomes the backup only after enabling it', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        const state = {user: '1', provider: 'openrouter', model: 'fixture/model', codex_model: 'fixture-codex',
            requests: [], tasks: [], openrouter_connected: true};
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'openrouter-models') return {models: [{id: 'fixture/model', name: 'Main model'}]};
            if (action === 'save-backup-model') {
                assert.deepEqual(data, {provider: 'codex', model: 'fixture-codex'});
                state.backup_model = data;
                return {ok: true};
            }
            if (action === 'clear-backup-model') {
                state.backup_model = null;
                return {ok: true};
            }
            assert.fail(action);
        });
        await page.goto('https://tinybot.test/mini');
        await page.locator('[data-tab="model"]').click();
        const backup = page.locator('section').filter({has: page.getByRole('heading', {name: 'Backup model', exact: true})});
        await backup.getByText('Off', {exact: true}).waitFor();
        await backup.getByRole('button', {name: 'Use Codex as backup'}).click();
        await backup.getByText('ChatGPT for Codex · fixture-codex', {exact: true}).waitFor();
        await backup.getByRole('button', {name: 'Turn off backup model'}).click();
        await backup.getByText('Off', {exact: true}).waitFor();
        assert.equal(state.provider, 'openrouter');
    } finally {
        await browser.close();
    }
});

test('Docblock sign-in and note link management stay inside the signed Mini App', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        const state = {user: '1', provider: 'codex', requests: [], tasks: [], connectors: []};
        const connector = {id: 'docs', name: 'Docblock', auth: 'oauth', endpoint: 'https://mcp.docblock.dev/'};
        const share = {id: 'test-share', target: 'minutes/weekly', expires_at: '2026-12-01T08:00:00Z'};
        let revoked = false;
        let approved = false;
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'docblock-oauth') {
                assert.deepEqual(data, state.connectors.length ? {id: 'docs'} : {});
                return {consent_url: 'https://docblock.dev/oauth/authorize?state=synthetic'};
            }
            if (action === 'docblock-workspaces') {
                assert.deepEqual(data, {id: 'docs'});
                return {workspaces: [{id: 'my-workspace', name: 'Personal', role: 'owner'}]};
            }
            if (action === 'docblock-shares') {
                assert.deepEqual(data, {id: 'docs', workspace: 'my-workspace'});
                return {shares: revoked ? [] : [share]};
            }
            if (action === 'revoke-docblock-share') {
                assert.deepEqual(data, {id: 'docs', workspace: 'my-workspace', share: 'test-share'});
                revoked = true;
                return {ok: true};
            }
            if (action === 'approve-docblock-share') {
                assert.deepEqual(data, {id: 'approval', expires_in_days: 14});
                approved = true;
                state.requests = [];
                return {ok: true};
            }
            assert.fail(action);
        }, 'window.Telegram = {WebApp: {initData: "fixture", ready() {}, openLink(url) { window.openedLink = url; }}};');
        await page.goto('https://tinybot.test/mini?connect=docblock');
        await page.getByRole('button', {name: 'Connect Docblock'}).click();
        await page.getByRole('button', {name: 'Open Docblock sign-in'}).click();
        assert.equal(await page.evaluate(() => window.openedLink), 'https://docblock.dev/oauth/authorize?state=synthetic');
        state.connectors = [connector];
        state.docblock_access = {docs: 'read'};
        await page.getByRole('button', {name: 'Check connection'}).click();
        await page.getByText('Read-only access').waitFor();
        assert.equal(await page.getByRole('button', {name: 'Manage public note links'}).count(), 0);
        await page.getByRole('button', {name: 'Sign in again'}).click();
        state.docblock_access.docs = 'write';
        state.requests = [{id: 'approval', kind: 'approve', content: {operation: 'Publish Docblock note', mcp: true,
            docblock: {workspace: 'Personal', target: 'minutes/weekly', change: 'Later edits appear at the same link.', expires_in_days: 7}}}];
        await page.getByRole('button', {name: 'Check connection'}).click();
        await page.getByRole('button', {name: 'Manage public note links'}).click();
        await page.getByRole('button', {name: 'Revoke link', exact: true}).click();
        await page.getByRole('button', {name: 'Confirm revocation'}).click();
        await page.getByText('Link revoked.').waitFor();
        assert.equal(revoked, true);
        await page.locator('[data-tab="home"]').click();
        assert.equal(await page.getByRole('button', {name: 'Always approve'}).count(), 0);
        const expiry = page.getByLabel('Expires after (days)');
        assert.equal(await expiry.inputValue(), '7');
        await expiry.fill('14');
        await page.getByRole('button', {name: 'Approve once'}).click();
        await page.getByText('Publication approved.').waitFor();
        assert.equal(approved, true);
    } finally {
        await browser.close();
    }
});

test('a self-hosted AnythingMCP gateway signs in from Connections and lists its tools as text', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 620}});
        const page = await context.newPage();
        const endpoint = 'https://mcp.example.test/mcp/workspace';
        const consent = 'https://mcp.example.test/oauth/authorize?state=fixture';
        const connector = {id: 'gateway', name: 'AnythingMCP', endpoint, kind: 'anythingmcp', auth: 'pending', transport: 'streamable-http'};
        const state = {user: '1', provider: 'codex', requests: [], tasks: [], connectors: [], anything_mcp_origin: 'https://mcp.example.test'};
        let registrations = 0;
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'add-anything-mcp') {
                assert.deepEqual(data, {endpoint});
                registrations++;
                state.connectors = [connector];
                return {id: connector.id};
            }
            if (action === 'mcp-oauth') {
                assert.deepEqual(data, {id: connector.id});
                return {consent_url: consent};
            }
            if (action === 'mcp-tools') {
                assert.equal(connector.auth, 'oauth');
                assert.deepEqual(data, {id: connector.id});
                return {total: 2, tools: [{name: 'list_projects', description: '<b>List projects</b>'}]};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        }, `window.Telegram = {WebApp: {
            initData: 'fixture', ready() {}, openLink(url) { window.openedLink = url; },
        }};`);
        await page.goto('https://tinybot.test/');
        await page.getByRole('button', {name: 'Connections', exact: true}).click();
        await page.getByRole('heading', {name: 'Connect AnythingMCP'}).waitFor();
        const url = page.getByLabel('Gateway MCP URL', {exact: true});
        assert.equal(await url.inputValue(), 'https://mcp.example.test/mcp');
        await page.getByRole('button', {name: 'Open gateway', exact: true}).click();
        assert.equal(await page.evaluate(() => window.openedLink), 'https://mcp.example.test');
        await page.evaluate(() => { delete window.openedLink; });
        await url.fill(endpoint);
        await page.getByRole('button', {name: 'Continue', exact: true}).click();
        const card = page.locator('#connection-gateway');
        await card.getByText(endpoint, {exact: true}).waitFor();
        assert.equal(await card.evaluate(element => element === document.activeElement), true);
        assert.equal(await card.getByRole('button', {name: 'Connect without a token'}).count(), 0);
        await card.getByRole('button', {name: 'Sign in to AnythingMCP'}).click();
        const open = card.getByRole('button', {name: 'Open gateway sign-in'});
        await open.waitFor();
        assert.equal(await page.evaluate(() => window.openedLink), undefined);
        await open.click();
        assert.equal(await page.evaluate(() => window.openedLink), consent);
        connector.auth = 'oauth';
        await card.getByRole('button', {name: 'Check connection'}).click();
        await card.getByText('Signed in with OAuth', {exact: true}).waitFor();
        await card.getByRole('button', {name: 'View tools'}).click();
        await card.getByRole('heading', {name: 'Showing 1 of 2 tools'}).waitFor();
        await card.getByText('list_projects', {exact: true}).waitFor();
        await card.getByText('<b>List projects</b>', {exact: true}).waitFor();
        assert.equal(await card.locator('b').count(), 0);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
        assert.equal(registrations, 1);
    } finally {
        await browser.close();
    }
});

test('AnythingMCP API keys reuse the connection and empty the password field after a failed verification', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        const connector = {id: 'gateway', name: 'AnythingMCP', endpoint: 'https://mcp.example.test/mcp', kind: 'anythingmcp', auth: 'pending', transport: 'streamable-http'};
        const state = {user: '1', provider: 'codex', requests: [], tasks: [], connectors: [connector]};
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'mcp-token') {
                assert.deepEqual(data, {id: 'gateway', token: 'amcp_local_fixture', header: 'X-API-Key', prefix: ''});
                connector.auth = 'api';
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        });
        await page.route('**/api/mini', route => {
            const {action, data} = route.request().postDataJSON();
            if (action === 'mcp-token' && data.token === 'wrong-key') {
                return route.fulfill({status: 400, json: {error: 'Check the MCP API key.'}});
            }
            return route.fallback();
        });
        await page.goto('https://tinybot.test/');
        await page.getByRole('button', {name: 'Connections', exact: true}).click();
        const card = page.locator('#connection-gateway');
        await card.getByText('Use an MCP API key', {exact: true}).click();
        const key = card.getByLabel('MCP API key', {exact: true});
        const connect = card.getByRole('button', {name: 'Connect with API key'});
        await key.fill('wrong-key');
        await connect.click();
        await page.getByText('Check the MCP API key.', {exact: true}).waitFor();
        assert.equal(await key.inputValue(), '');
        assert.equal(await card.getByLabel('Token header', {exact: false}).count(), 0);
        await key.fill('amcp_local_fixture');
        await connect.click();
        await card.getByText('Connected with an API key', {exact: true}).waitFor();
        await card.getByText('Reconnect', {exact: true}).click();
        await card.getByText('Use an MCP API key', {exact: true}).click();
        assert.equal(await key.inputValue(), '');
    } finally {
        await browser.close();
    }
});

test('a Telegram profile launch requests fullscreen after verification and can fall back to expansion', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        for (const [mode, expected] of [
            ['fullscreen', ['ready', 'fullscreen']],
            ['unsupported', ['ready', 'fullscreen', 'expand']],
            ['throws', ['ready', 'fullscreen', 'expand']],
            ['older', ['ready', 'expand']],
        ]) {
            const context = await browser.newContext({viewport: {width: 360, height: 720}});
            const page = await context.newPage();
            await mockMiniApp(context, () => ({user: '1', provider: 'codex', requests: [], tasks: []}), `
                const mode = ${JSON.stringify(mode)};
                const events = new Map();
                window.calls = [];
                window.Telegram = {WebApp: {
                    initData: new URLSearchParams(location.hash.slice(1)).get('tgWebAppData'),
                    onEvent(name, callback) { events.set(name, callback); },
                    isVersionAtLeast(version) { return version === '8.0' && mode !== 'older'; },
                    ready() { window.calls.push('ready'); },
                    requestFullscreen() {
                        window.calls.push('fullscreen');
                        if (mode === 'unsupported') events.get('fullscreenFailed')({error: 'UNSUPPORTED'});
                        if (mode === 'throws') throw new Error('Fullscreen unavailable');
                    },
                    expand() { window.calls.push('expand'); },
                }};
                document.documentElement.style.setProperty('--tg-safe-area-inset-top', '20px');
                document.documentElement.style.setProperty('--tg-content-safe-area-inset-top', '32px');
            `);
            await page.goto('https://tinybot.test/#tgWebAppData=fixture');
            await page.getByRole('heading', {name: 'Requests', exact: true}).waitFor();
            assert.deepEqual(await page.evaluate(() => window.calls), expected);
            assert.equal(await page.locator('.shell').evaluate(element => getComputedStyle(element).paddingTop), '68px');
            await page.getByRole('button', {name: 'Refresh', exact: true}).click();
            await page.getByRole('heading', {name: 'Requests', exact: true}).waitFor();
            assert.deepEqual(await page.evaluate(() => window.calls), expected);
            await context.close();
        }
    } finally {
        await browser.close();
    }
});

test('a launch without accepted Telegram data shows instructions and leaves settings closed', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        for (const init of ['', 'fixture']) {
            const context = await browser.newContext();
            const page = await context.newPage();
            await mockMiniApp(context, () => assert.fail('Unverified API request'), `
                window.Telegram = {WebApp: {
                    initData: ${JSON.stringify(init)},
                    ready() {},
                    expand() { window.expanded = true; },
                }};
            `);
            if (init) {
                await page.route('**/api/mini', route => route.fulfill({status: 403, json: {error: 'Forbidden'}}));
            }
            await page.goto('https://tinybot.test/');
            await page.getByText('Open TinyBot from its Telegram profile', {exact: false}).waitFor();
            assert.equal(await page.getByRole('navigation', {name: 'TinyBot settings'}).count(), 0);
            assert.equal(await page.getByRole('button', {name: 'Refresh', exact: true}).count(), 0);
            assert.equal(await page.evaluate(() => window.expanded), undefined);
            await context.close();
        }
    } finally {
        await browser.close();
    }
});

test('settings follow live Telegram colors and open the GitHub source in Telegram', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({colorScheme: 'light', viewport: {width: 360, height: 720}});
    const page = await context.newPage();
    const state = {user: '1', provider: 'codex', requests: [], tasks: []};
    await mockMiniApp(context, () => state, `
        let themeChanged;
        window.Telegram = {WebApp: {
            initData: 'fixture',
            ready() {},
            isVersionAtLeast(version) { return version === '6.1'; },
            onEvent(name, callback) { if (name === 'themeChanged') themeChanged = callback; },
            setHeaderColor(color) { window.headerColor = color; },
            setBackgroundColor(color) { window.backgroundColor = color; },
            openLink(url) { window.openedLink = url; },
        }};
        window.setTheme = (scheme, colors) => {
            for (const [name, color] of Object.entries(colors)) {
                document.documentElement.style.setProperty('--tg-theme-' + name, color);
            }
            window.Telegram.WebApp.colorScheme = scheme;
            themeChanged?.();
        };
        window.setTheme('light', {
            'secondary-bg-color': '#f1edf4', 'section-bg-color': '#faf7fb',
            'button-color': '#67439b', 'button-text-color': '#ffffff', 'link-color': '#5f3c82',
        });
    `);
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('heading', {name: 'Requests', exact: true}).waitFor();
        const colors = () => page.evaluate(() => ({
            bodyBackground: getComputedStyle(document.body).backgroundColor,
            cardBackground: getComputedStyle(document.querySelector('.card')).backgroundColor,
            selectedBackground: getComputedStyle(document.querySelector('nav button[aria-current]')).backgroundColor,
            browserScheme: getComputedStyle(document.documentElement).colorScheme,
        }));
        assert.deepEqual(await colors(), {
            bodyBackground: 'rgb(241, 237, 244)',
            cardBackground: 'rgb(250, 247, 251)',
            selectedBackground: 'rgb(103, 67, 155)',
            browserScheme: 'light',
        });
        await page.evaluate(() => window.setTheme('dark', {
            'secondary-bg-color': '#0e1a23', 'section-bg-color': '#192936',
            'button-color': '#97d3f8', 'button-text-color': '#102536', 'link-color': '#a9ddff',
        }));
        assert.deepEqual(await colors(), {
            bodyBackground: 'rgb(14, 26, 35)',
            cardBackground: 'rgb(25, 41, 54)',
            selectedBackground: 'rgb(151, 211, 248)',
            browserScheme: 'dark',
        });
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 360);
        assert.deepEqual(await page.evaluate(() => [window.headerColor, window.backgroundColor]), [
            'secondary_bg_color', 'secondary_bg_color',
        ]);
        const source = page.getByRole('link', {name: 'Source on GitHub', exact: true});
        assert.equal(await source.getAttribute('href'), 'https://github.com/abcdan/tinybot');
        assert.equal(await source.evaluate(element => getComputedStyle(element).color), 'rgb(169, 221, 255)');
        await source.click();
        assert.equal(await page.evaluate(() => window.openedLink), 'https://github.com/abcdan/tinybot');
    } finally {
        await browser.close();
    }
});

test('settings have readable light and dark palettes without Telegram theme data', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({colorScheme: 'dark'});
    const page = await context.newPage();
    await mockMiniApp(context, () => ({user: '1', provider: 'codex', requests: [], tasks: []}));
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('heading', {name: 'Requests', exact: true}).waitFor();
        const colors = () => page.locator('body').evaluate(body => {
            const {backgroundColor, color} = getComputedStyle(body);
            return {backgroundColor, color};
        });
        assert.deepEqual(await colors(), {backgroundColor: 'rgb(16, 25, 35)', color: 'rgb(233, 240, 247)'});
        await page.emulateMedia({colorScheme: 'light'});
        assert.deepEqual(await colors(), {backgroundColor: 'rgb(237, 242, 247)', color: 'rgb(24, 38, 52)'});
    } finally {
        await browser.close();
    }
});

test('browser proxy settings are operator only and clear the private URL after saving or error', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({viewport: {width: 360, height: 720}});
    const page = await context.newPage();
    const state = {user: '1', provider: 'codex', requests: [], tasks: [], browser_proxy: null};
    const proxyUrl = 'socks5://user:fixture-password@proxy.example:1080';
    await mockMiniApp(context, ({action, data}) => {
        switch (action) {
            case 'status': return state;
            case 'save-browser-proxy':
                assert.deepEqual(data, {url: proxyUrl});
                state.browser_proxy = {source: 'saved', overridden: true};
                return {ok: true};
            case 'disable-browser-proxy':
                assert.deepEqual(data, {});
                state.browser_proxy = {source: 'direct', overridden: true};
                return {ok: true};
            case 'remove-browser-proxy':
                assert.deepEqual(data, {});
                state.browser_proxy = {source: 'environment', overridden: false};
                return {ok: true};
            default: assert.fail('Unexpected Mini App action ' + action);
        }
    });
    await page.route('**/api/mini', route => {
        const {action, data} = route.request().postDataJSON();
        if (action === 'save-browser-proxy' && data.url === 'https://bad-proxy.example') {
            return route.fulfill({status: 400, json: {error: 'Use an HTTP or SOCKS5 proxy URL.'}});
        }
        return route.fallback();
    });
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('heading', {name: 'Requests', exact: true}).waitFor();
        const tab = page.getByRole('button', {name: 'Browser', exact: true});
        assert.equal(await tab.count(), 0);
        const refresh = page.getByRole('button', {name: 'Refresh', exact: true});
        state.browser_proxy = {source: 'unavailable'};
        await refresh.click();
        await tab.click();
        await page.getByText('Proxy settings are unavailable. Tap Refresh to try again.').waitFor();
        const url = page.getByLabel('Proxy URL');
        assert.equal(await url.count(), 0);

        state.browser_proxy = {source: 'direct'};
        await refresh.click();
        await page.getByText('Using a direct browser connection.').waitFor();
        assert.equal(await url.getAttribute('type'), 'password');
        const save = page.getByRole('button', {name: 'Save proxy'});
        await url.fill('https://bad-proxy.example');
        await save.click();
        await page.getByText('Use an HTTP or SOCKS5 proxy URL.', {exact: true}).waitFor();
        assert.equal(await url.inputValue(), '');

        await url.fill(proxyUrl);
        await save.click();
        await page.getByText('Using the saved proxy for browser traffic.').waitFor();
        assert.equal(await url.inputValue(), '');
        assert.equal((await page.locator('body').innerText()).includes('fixture-password'), false);
        await page.getByRole('button', {name: 'Use direct connection'}).click();
        await page.getByText('Using a direct browser connection.').waitFor();
        const restore = page.getByRole('button', {name: 'Restore deployment setting'});
        await restore.click();
        await page.getByText('Using the proxy configured in deployment settings.').waitFor();
        assert.equal(await restore.count(), 0);

        state.browser_proxy = null;
        await refresh.click();
        await page.getByRole('heading', {name: 'Requests', exact: true}).waitFor();
        assert.equal(await tab.count(), 0);
    } finally {
        await browser.close();
    }
});

test('Email settings keep inbox handling opt-in and passwords out of the loaded form', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 650}, reducedMotion: 'reduce'});
        const page = await context.newPage();
        const state = {user: '1', provider: 'codex', requests: [], tasks: [], mail_accounts: []};
        const id = 'c01857d1-c96a-43d2-b285-ab2636b4ac61';
        let updates = 0;
        const account = {
            label: 'Work', address: 'user@example.com',
            imap: {host: 'imap.example.com', port: 993, username: 'user@example.com'},
            smtp: {host: 'smtp.example.com', port: 465, username: 'user@example.com'},
            folders: [
                {name: 'INBOX', automate: false, instruction: null},
                {name: 'Projects', automate: true, instruction: 'Summarize new mail from this folder.'}
            ]
        };
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            if (action === 'save-mail-account') {
                const creating = updates++ === 0;
                assert.equal(data.id, creating ? undefined : id);
                assert.deepEqual(data.config, {
                    ...account,
                    label: creating ? 'Work' : 'Projects mail',
                    imap: {...account.imap, password: creating ? 'test-imap-secret' : ''},
                    smtp: {...account.smtp, password: creating ? 'test-smtp-secret' : ''}
                });
                state.mail_accounts = [{...account, id, label: data.config.label}];
                return {id};
            }
            if (action === 'remove-mail-account') {
                assert.deepEqual(data, {id});
                state.mail_accounts = [];
                return {ok: true};
            }
            assert.fail(`Unexpected Mini App action ${action}`);
        });
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Email', exact: true}).click();
        await page.getByRole('button', {name: 'Add email account'}).click();
        await page.getByLabel('Account name').fill('Work');
        await page.getByLabel('From email').fill('user@example.com');
        await page.getByLabel('IMAP host').fill('imap.example.com');
        await page.getByLabel('IMAP username').fill('user@example.com');
        await page.getByLabel('IMAP password').fill('test-imap-secret');
        assert.equal(await page.getByLabel('IMAP password').getAttribute('type'), 'password');
        assert.equal(await page.getByLabel('Folder name').inputValue(), 'INBOX');
        const automate = page.getByRole('checkbox', {name: 'Let the agent handle new mail'});
        assert.equal(await automate.isChecked(), false);
        await page.getByRole('button', {name: 'Add watched folder'}).click();
        await page.getByLabel('Folder name').nth(1).fill('Projects');
        await automate.nth(1).check();
        await page.locator('.mail-watch-entry').nth(1).getByLabel('Handling instructions (optional)').fill('Summarize new mail from this folder.');
        await page.getByRole('checkbox', {name: 'Send via SMTP'}).check();
        await page.getByLabel('SMTP host').fill('smtp.example.com');
        await page.getByLabel('SMTP security').selectOption('465');
        await page.getByLabel('SMTP username').fill('user@example.com');
        await page.getByLabel('SMTP password').fill('test-smtp-secret');
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
        await page.getByRole('button', {name: 'Save account'}).click();
        await page.getByRole('heading', {name: 'Work', exact: true}).waitFor();
        await page.getByRole('listitem').filter({hasText: 'INBOX'}).getByText('Notify only').waitFor();
        await page.getByRole('listitem').filter({hasText: 'Projects'}).getByText('Automatic handling').waitFor();

        await page.getByRole('button', {name: 'Edit account'}).click();
        assert.equal(await page.getByLabel('IMAP password').inputValue(), '');
        assert.equal(await page.getByLabel('SMTP password').inputValue(), '');
        await page.getByLabel('Account name').fill('Projects mail');
        await page.getByRole('button', {name: 'Save account'}).click();
        await page.getByRole('heading', {name: 'Projects mail', exact: true}).waitFor();
        assert.equal(updates, 2);
        await page.getByRole('button', {name: 'Remove account'}).click();
        await page.getByRole('button', {name: 'Keep account'}).click();
        assert.equal(state.mail_accounts.length, 1);
        await page.getByRole('button', {name: 'Remove account'}).click();
        await page.getByRole('button', {name: 'Remove account'}).click();
        await page.getByText('Email account removed.', {exact: true}).waitFor();
        assert.equal(state.mail_accounts.length, 0);
    } finally {
        await browser.close();
    }
});

test('Email approval shows the exact plain-text draft and offers a one-time approval', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        const draft = 'From: user@example.com\nTo: reviewer@example.com\nSubject: Fixture\n\nReview <strong>literal text</strong>.';
        const state = {user: '1', provider: 'codex', tasks: [], requests: [
            {id: 'draft', kind: 'email-send', content: {operation: 'Send email', explanation: draft}}
        ]};
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return state;
            assert.equal(action, 'approve');
            assert.deepEqual(data, {id: 'draft'});
            state.requests = [];
            return {ok: true};
        });
        await page.goto('https://tinybot.test/mini');
        const request = page.locator('#view > .card').filter({has: page.getByRole('heading', {name: 'Send email'})});
        await request.waitFor();
        assert.equal(await request.locator('pre').textContent(), draft);
        assert.equal(await request.getByRole('button', {name: 'Always approve'}).count(), 0);
        assert.equal(await request.getByRole('button', {name: 'Deny'}).count(), 1);
        await request.getByRole('button', {name: 'Approve once'}).click();
        await page.getByText('Send approved.', {exact: true}).waitFor();
    } finally {
        await browser.close();
    }
});

test('memory settings page by scope and manage group notes with the signed API', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({viewport: {width: 360, height: 720}});
    const page = await context.newPage();
    const state = {
        user: '1', provider: 'codex', requests: [], tasks: [], time_zone: 'UTC',
        agents: [
            {id: 'writer', owner: '1', name: 'Writer'},
            {id: 'research', name: 'Shared research helper'},
        ],
    };
    const created_at = 1731933900;
    const groupText = 'Use concise updates.\nKeep <b>examples</b> as text.';
    let groupNotes = [];
    await mockMiniApp(context, ({action, data}) => {
        switch (action) {
            case 'status': return state;
            case 'list-memories':
                if (data.scope === 'group') return {entries: groupNotes};
                if (data.scope === 'agent') {
                    assert.deepEqual(data, {scope: 'agent', agent: 'writer'});
                    return {entries: [{id: 7, scope: 'agent', agent: 'writer', created_at, body: 'Writer preferences'}]};
                }
                assert.equal(data.scope, 'private');
                if (data.before != null) {
                    assert.equal(data.before, 10);
                    return {entries: [{id: 9, scope: 'private', created_at, body: 'Older Chief memory'}]};
                }
                return {entries: [{id: 10, scope: 'private', created_at, body: 'Chief preferences'}], next_before: 10};
            case 'save-memory':
                assert.deepEqual(data, {scope: 'group', body: groupText});
                groupNotes = [{id: 11, scope: 'group', created_at, body: groupText}];
                return {ok: true};
            case 'delete-memory':
                assert.deepEqual(data, {id: 11});
                groupNotes = [];
                return {ok: true};
            default: assert.fail('Unexpected Mini App action ' + action);
        }
    });
    let unavailable = true;
    await page.route('**/api/mini', route => {
        const {action, data} = route.request().postDataJSON();
        if (unavailable && action === 'list-memories' && data.scope === 'group') {
            unavailable = false;
            return route.fulfill({status: 503, json: {error: 'Memory settings unavailable.'}});
        }
        return route.fallback();
    });
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Memory', exact: true}).click();
        const scope = page.getByRole('combobox', {name: 'Memory scope'});
        await page.getByText('Chief preferences', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Load older memories'}).click();
        await page.getByText('Older Chief memory', {exact: true}).waitFor();
        assert.equal(await scope.getByRole('option', {name: 'Helper · Shared research helper'}).count(), 0);

        await scope.selectOption('group');
        await page.getByText('Memory settings unavailable.', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Retry loading memories'}).click();
        await page.getByText('No memories saved here yet.').waitFor();
        const note = page.getByRole('textbox', {name: 'Memory', exact: true});
        await note.fill(groupText);
        await page.getByRole('button', {name: 'Save memory'}).click();
        const entry = page.locator('.memory-note');
        await entry.waitFor();
        assert.equal(await entry.textContent(), groupText);
        assert.equal(await entry.locator('b').count(), 0);
        assert.equal(await note.inputValue(), '');
        await page.getByRole('button', {name: 'Refresh', exact: true}).click();
        await entry.waitFor();
        assert.equal(await scope.inputValue(), 'group');
        await page.getByRole('button', {name: 'Delete memory'}).click();
        await page.getByText('No memories saved here yet.').waitFor();

        await scope.selectOption('agent:writer');
        await page.getByText('Writer preferences', {exact: true}).waitFor();
        assert.equal(await page.getByText('Chief preferences', {exact: true}).count(), 0);
    } finally {
        await browser.close();
    }
});

test('a generated Mini App uses shared controls and submits details for masked review', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let received;
    await mockMiniApp(context, request => {
        if (request.action === 'submit-form') received = request.data;
        const details = {destination: 'https://forms.example/submit'};
        const requests = received ?
            [{id: 'review', kind: 'form-submit', content: {...details, preview: 'Email address: ••••'}}] :
            [{id: 'entry', kind: 'form', content: {...details, fields: [{
                name: 'email', label: 'Email address', type: 'email', required: true,
                options: [], private: true, hint: 'For example, jamie@example.com.'
            }]}}];
        return {user: '1', provider: 'openrouter', requests, tasks: []};
    });
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByText('For example, jamie@example.com.').waitFor();
        const email = page.locator('input[name=email]');
        assert.equal(await email.getAttribute('type'), 'email');
        await email.fill('smoke@example.net');
        await page.getByRole('button', {name: 'Continue to review'}).click();
        await page.getByText('Email address: ••••').waitFor();
        assert.deepEqual(received, {id: 'entry', values: {email: 'smoke@example.net'}});
        assert.equal((await page.locator('body').innerText()).includes('smoke@example.net'), false);
        assert.deepEqual(errors, []);
    } finally {
        await browser.close();
    }
});

test('a generated focused form submits text and a selected option from Telegram', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext();
    const page = await context.newPage();
    const requests = [];
    const fields = [
        {name: 'project', label: 'Project name', type: 'text', required: true, options: [], private: false},
        {name: 'priority', label: 'Priority', type: 'select', required: true, options: ['Normal', 'High'], private: false},
    ];
    await mockMiniApp(context, ({action, data}) => {
        requests.push({action, data});
        if (action === 'get-form') return {id: 'form-1', kind: 'elicitation', content: {fields}};
        assert.equal(action, 'answer-form');
        return {ok: true};
    });
    try {
        await page.goto('https://tinybot.test/mini?form=form-1');
        await page.getByRole('heading', {name: 'Answer a few questions', exact: true}).waitFor();
        assert.equal(await page.getByRole('navigation', {name: 'TinyBot settings'}).isVisible(), false);
        assert.equal(await page.getByRole('link', {name: 'Source on GitHub'}).isVisible(), false);
        await page.getByRole('textbox', {name: 'Project name'}).fill('Weekend plans');
        await page.getByRole('combobox', {name: 'Priority'}).selectOption('High');
        await page.getByRole('button', {name: 'Send answers'}).click();
        await page.getByRole('heading', {name: 'Answers sent', exact: true}).waitFor();
        assert.deepEqual(requests, [
            {action: 'get-form', data: {id: 'form-1'}},
            {action: 'answer-form', data: {id: 'form-1', values: {project: 'Weekend plans', priority: 'High'}}},
        ]);
        assert.equal(await page.getByRole('textbox', {name: 'Project name'}).count(), 0);
        await page.getByRole('button', {name: 'Back to chat'}).click();
        await page.waitForFunction(() => window.miniAppClosed === true);
    } finally {
        await browser.close();
    }
});

test('TinyBot settings form shows exact changes before explicit approval', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 640}});
        const page = await context.newPage();
        const calls = [];
        const before = 'Previous Chief';
        const proposed = '<b>New Chief</b>';
        const draft = {
            id: 'settings-form-1',
            kind: 'settings-form',
            content: {operation: 'Configure TinyBot', fields: [
                {name: 'chief_name', label: 'Chief name', type: 'text', value: before, required: true},
                {name: 'web_search', label: 'Web search', type: 'select',
                    options: ['enabled', 'disabled'], value: 'enabled', required: true},
            ]},
        };
        const review = {
            id: 'settings-review-1',
            kind: 'settings-approve',
            content: {operation: 'Approve settings changes', changes: [
                {label: 'Chief name', before, after: proposed},
                {label: 'Web search', before: 'Enabled', after: 'Disabled'},
            ]},
        };
        await mockMiniApp(context, ({action, data}) => {
            calls.push({action, data});
            if (action === 'get-form') return data.id === draft.id ? draft : review;
            if (action === 'answer-settings-form') return review;
            assert.equal(action, 'approve-settings');
            return {ok: true};
        });
        await page.goto('https://tinybot.test/mini?form=settings-form-1');
        const name = page.getByRole('textbox', {name: 'Chief name'});
        const search = page.getByRole('combobox', {name: 'Web search'});
        await name.waitFor();
        assert.equal(await name.inputValue(), before);
        assert.equal(await search.inputValue(), 'enabled');
        assert.equal(await page.getByRole('button', {name: 'Approve changes'}).count(), 0);
        await name.fill(proposed);
        await search.selectOption('disabled');
        await page.getByRole('button', {name: 'Review changes'}).click();
        await page.getByRole('heading', {name: 'Approve settings changes'}).waitFor();
        assert.deepEqual(calls, [
            {action: 'get-form', data: {id: draft.id}},
            {action: 'answer-settings-form', data: {
                id: draft.id, values: {chief_name: proposed, web_search: 'disabled'},
            }},
        ]);
        await page.getByText(before, {exact: true}).waitFor();
        await page.getByText(proposed, {exact: true}).waitFor();
        assert.equal(await page.locator('.settings-changes b').count(), 0);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
        await page.reload();
        await page.getByText(proposed, {exact: true}).waitFor();
        assert.deepEqual(calls.at(-1), {action: 'get-form', data: {id: review.id}});
        await page.getByRole('button', {name: 'Approve changes'}).click();
        await page.getByRole('heading', {name: 'Settings updated.'}).waitFor();
        assert.deepEqual(calls.at(-1), {action: 'approve-settings', data: {id: review.id}});
    } finally {
        await browser.close();
    }
});

test('a pending TinyBot settings review can be declined from Requests', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        let pending = true;
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return {
                user: '1', provider: 'codex', tasks: [], requests: pending ? [{
                    id: 'pending-settings',
                    kind: 'settings-approve',
                    content: {changes: [
                        {label: 'Time zone', before: 'UTC', after: 'Europe/Amsterdam'},
                    ]},
                }] : [],
            };
            assert.equal(action, 'deny');
            assert.deepEqual(data, {id: 'pending-settings'});
            pending = false;
            return {ok: true};
        });
        await page.goto('https://tinybot.test/mini');
        await page.getByText('Europe/Amsterdam', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Decline', exact: true}).click();
        await page.getByText('Settings changes declined.', {exact: true}).waitFor();
        await page.getByText('You’re all caught up.', {exact: true}).waitFor();
    } finally {
        await browser.close();
    }
});

test('helpers can be managed while only Chief tasks can be selected in chat', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext();
    const page = await context.newPage();
    const state = {
        user: '1', provider: 'codex', requests: [], active_task: null,
        tasks: [
            {id: 'chief-task', agent: null, title: 'Chief research task', state: 'idle'},
            {id: 'shared-chief-task', agent: 'research', title: 'Chief task in a shared workspace', state: 'idle'}
        ],
        agents: [
            {id: 'research', owner: '1', name: 'Research helper', instructions: 'Find sources.', tools: '[]'},
            {id: 'writer', name: 'Shared writing helper'}
        ],
        members: [['research', '2']], memory: [], connectors: [], grants: []
    };
    await mockMiniApp(context, ({action, data}) => {
        if (action === 'status') return state;
        assert.equal(action, 'select-task');
        assert.deepEqual(data, {task: 'chief-task'});
        state.active_task = data.task;
        return {ok: true};
    });
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('heading', {name: 'Chief research task'}).waitFor();
        await page.getByRole('heading', {name: 'Chief task in a shared workspace'}).waitFor();
        await page.locator('.card').filter({has: page.getByRole('heading', {name: 'Chief research task'})})
            .getByRole('button', {name: 'Continue', exact: true}).click();
        await page.getByText('Current Chief task', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Helpers', exact: true}).click();
        await page.getByRole('heading', {name: 'Research helper', exact: true}).waitFor();
        assert.equal(await page.getByRole('heading', {name: 'Shared writing helper'}).count(), 1);
        assert.equal(await page.getByRole('button', {name: 'Use in chat'}).count(), 0);
        assert.equal(await page.getByRole('button', {name: 'Save helper'}).count(), 1);
        await page.getByText('Edit helper', {exact: true}).click();
        await page.getByRole('button', {name: 'Save changes'}).waitFor();
        await page.getByText('Sharing', {exact: true}).click();
        await page.getByRole('button', {name: 'Revoke', exact: true}).waitFor();
    } finally {
        await browser.close();
    }
});

test('one OpenRouter key lists task models while retaining Codex pairing', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext();
    const page = await context.newPage();
    const requests = [];
    const state = {
        provider: 'codex', model: 'codex-fixture', codex_model: 'codex-fixture',
        openrouter_connected: false, requests: [], tasks: []
    };
    const models = [
        {id: 'provider/one', name: 'First model'},
        {id: 'provider/two', name: 'Second model'}
    ];
    const key = 'sk-or-fixture';
    await mockMiniApp(context, ({action, data}) => {
        requests.push({action, data});
        switch (action) {
            case 'status': return state;
            case 'openrouter-key':
                assert.deepEqual(data, {key});
                state.openrouter_connected = true;
                return {models};
            case 'openrouter-models':
                assert(state.openrouter_connected);
                return {models};
            case 'openrouter-model':
                assert.deepEqual(data, {model: 'provider/two'});
                state.provider = 'openrouter';
                state.model = data.model;
                return {ok: true};
            case 'codex-select':
                state.provider = 'codex';
                state.model = state.codex_model;
                return {ok: true};
            case 'disconnect-openrouter':
                assert.equal(state.provider, 'codex');
                state.openrouter_connected = false;
                return {ok: true};
            default: assert.fail('Unexpected Mini App action ' + action);
        }
    });
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Model', exact: true}).click();
        const password = page.getByLabel('OpenRouter API key');
        await password.waitFor();
        assert.equal(await page.locator('input[type=password]').count(), 1);
        await password.fill(key);
        await page.getByRole('button', {name: 'Save API key'}).click();
        const selector = page.getByRole('combobox', {name: 'OpenRouter task model'});
        await selector.waitFor();
        assert.equal(await password.inputValue(), '');
        assert.equal(requests.some(({action}) => action === 'openrouter-models'), false);
        const modelListRequest = page.waitForRequest(request => request.url().endsWith('/api/mini') &&
            request.postDataJSON()?.action === 'openrouter-models');
        await page.getByRole('button', {name: 'Refresh', exact: true}).click();
        await modelListRequest;
        await selector.getByRole('option', {name: 'Second model (provider/two)'}).waitFor({state: 'attached'});
        assert.equal(requests.filter(({action}) => action === 'openrouter-models').length, 1);
        await selector.selectOption('provider/two');
        await page.getByRole('button', {name: 'Use OpenRouter for tasks'}).click();
        await page.getByText('OpenRouter · provider/two', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Use Codex for tasks'}).click();
        await page.getByText('ChatGPT for Codex · codex-fixture', {exact: true}).waitFor();
        await selector.waitFor();
        await page.getByRole('button', {name: 'Remove OpenRouter key'}).click();
        await password.waitFor();
        assert.equal(await selector.count(), 0);
        assert.equal(await page.getByText('Connected · codex-fixture', {exact: true}).count(), 1);
        assert.equal(requests.filter(({action}) => action === 'openrouter-key').length, 1);
    } finally {
        await browser.close();
    }
});

test('audit log retries, pages by message, and reveals chronological calls and results', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext();
    const page = await context.newPage();
    const started = Math.floor(Date.now() / 1000) - 60;
    const recent = {
        id: 42, task: 'task-42', agent: null, message: 'Find the weather', status: 'completed',
        started_at: started, finished_at: started + 5, entry_count: 3
    };
    const older = {
        id: 21, task: 'task-21', agent: 'Research assistant', message: 'Earlier message',
        status: 'waiting', started_at: started - 120, finished_at: null, entry_count: 0
    };
    const requests = [];
    await mockMiniApp(context, ({action, data}) => {
        requests.push({action, data});
        switch (action) {
            case 'status': return {user: '1', requests: [], tasks: [], time_zone: 'UTC'};
            case 'audit-sessions':
                return data.before === 42
                    ? {sessions: [older], next_before: null}
                    : {sessions: [recent], next_before: 42};
            case 'audit-session':
                assert.equal(data.id, 42);
                if (data.after === 2) return {session: recent, next_after: null, entries: [
                    {id: 3, at: started + 4, kind: 'tool', name: 'search_web', status: 'completed',
                        detail: {result: 'Sunny <script>safe text</script>'}}
                ]};
                return {session: recent, next_after: 2, entries: [
                    {id: 1, at: started, kind: 'model', name: 'fixture/model', status: 'started', detail: null},
                    {id: 2, at: started + 1, kind: 'tool', name: 'search_web', status: 'started',
                        detail: {arguments: {query: 'Weather forecast'}}}
                ]};
            default: assert.fail('Unexpected Mini App action ' + action);
        }
    });
    let failFirstList = true;
    await page.route('**/api/mini', route => {
        if (failFirstList && route.request().postDataJSON()?.action === 'audit-sessions') {
            failFirstList = false;
            return route.fulfill({status: 503, json: {error: 'Audit log unavailable.'}});
        }
        return route.fallback();
    });
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Audit', exact: true}).click();
        await page.getByText('Audit log unavailable.', {exact: true}).waitFor();
        await page.getByRole('button', {name: 'Retry loading audit log'}).click();
        await page.getByText('Find the weather', {exact: true}).waitFor();
        await page.getByRole('heading', {name: 'Recent messages'}).waitFor();
        assert.equal(requests.some(({action}) => action === 'audit-session'), false);
        await page.getByText('Find the weather', {exact: true}).click();
        await page.getByRole('button', {name: 'Load more entries'}).click();
        const entries = page.locator('.audit-timeline > li');
        await entries.nth(2).getByText('Search web', {exact: true}).waitFor();
        assert.deepEqual(await entries.locator('.audit-entry-name').allTextContents(), [
            'fixture/model', 'Search web', 'Search web',
        ]);
        assert.deepEqual(await entries.locator('.audit-status').allTextContents(), ['Started', 'Started', 'Completed']);
        assert.deepEqual(await entries.locator('time').evaluateAll(items => items.map(item => item.dateTime)),
            [started, started + 1, started + 4].map(time => new Date(time * 1000).toISOString()));

        const argumentsText = entries.nth(1).getByText(/Weather forecast/);
        const result = entries.nth(2).getByText('Sunny <script>safe text</script>', {exact: true});
        await argumentsText.waitFor({state: 'hidden'});
        await result.waitFor({state: 'hidden'});
        await entries.nth(1).locator('summary').click();
        await argumentsText.waitFor();
        await entries.nth(2).locator('summary').click();
        await result.waitFor();
        assert.equal(await entries.locator('script').count(), 0);
        assert.deepEqual(requests.filter(({action}) => action === 'audit-session').map(({data}) => data), [
            {id: 42}, {id: 42, after: 2}
        ]);
        await page.getByRole('button', {name: 'Load older messages'}).click();
        await page.getByText('Earlier message', {exact: true}).waitFor();
        assert.deepEqual(requests.filter(({action}) => action === 'audit-sessions').map(({data}) => data), [
            {}, {before: 42}
        ]);
        assert.equal(await page.getByRole('button', {name: 'Load older messages'}).count(), 0);
    } finally {
        await browser.close();
    }
});

test('audit JSON export downloads the whole run and offers a fallback for restricted clients', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 620}});
        await context.addInitScript(() => {
            Object.defineProperty(navigator, 'clipboard', {value: undefined});
        });
        const page = await context.newPage();
        const started = 1795380000;
        const run = {id: 42, message: 'Debug this run', started_at: started, status: 'completed', entry_count: 2};
        const entries = [
            {id: 1, at: started, kind: 'model_call', name: 'fixture/model', status: 'started', detail: null},
            {id: 2, at: started + 1, kind: 'tool', name: 'search_web', status: 'failed', detail: {category: 'timeout'}},
        ];
        const snapshot = {version: 1, session: run, entries, usage: {total: {calls: 1, total_tokens: 42, cost_usd: 0.02}}};
        const exports = [];
        await mockMiniApp(context, ({action, data}) => {
            switch (action) {
                case 'status': return {user: '1', requests: [], tasks: [], time_zone: 'UTC'};
                case 'audit-sessions': return {sessions: [run], next_before: null};
                case 'audit-session':
                    assert.deepEqual(data, {id: run.id});
                    return {session: run, entries: entries.slice(0, 1), next_after: 1};
                case 'audit-export':
                    exports.push(data);
                    return snapshot;
                default: assert.fail('Unexpected Mini App action ' + action);
            }
        });
        let failFirstExport = true;
        await page.route('**/api/mini', route => {
            if (failFirstExport && route.request().postDataJSON()?.action === 'audit-export') {
                failFirstExport = false;
                return route.fulfill({status: 503, json: {error: 'Could not export this session.'}});
            }
            return route.fallback();
        });
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Audit', exact: true}).click();
        const summary = page.getByText(run.message, {exact: true});
        await summary.click();
        await page.getByText('fixture/model', {exact: true}).waitFor();
        assert.equal(await page.locator('.audit-timeline > li').count(), 1);
        const save = page.getByRole('button', {name: 'Export JSON', exact: true});
        const copy = page.getByRole('button', {name: 'Copy JSON', exact: true});
        await save.click();
        await page.getByText('Could not export this session.', {exact: true}).waitFor();
        assert.equal(await copy.isVisible(), false);

        const pending = page.waitForEvent('download');
        await save.click();
        const download = await pending;
        assert.equal(download.suggestedFilename(), 'tinybot-audit-42.json');
        assert.deepEqual(JSON.parse(await readFile(await download.path(), 'utf8')), snapshot);
        assert.deepEqual(exports, [{id: 42}]);
        assert.equal(await page.locator('.audit-timeline > li').count(), 1);

        await copy.click();
        const output = page.getByRole('textbox', {name: 'Audit JSON', exact: true});
        assert.deepEqual(JSON.parse(await output.inputValue()), snapshot);
        assert.equal(await output.evaluate(element => element.selectionEnd - element.selectionStart), (await output.inputValue()).length);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
    } finally {
        await browser.close();
    }
});

test('audit usage shows provider, model and run totals without treating missing reports as zero', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({locale: 'en-US', viewport: {width: 320, height: 720}, colorScheme: 'light'});
    const page = await context.newPage();
    const started = 1795380000;
    const run = {id: 42, message: 'Look up the forecast', started_at: started, status: 'completed', entry_count: 1};
    const modelName = 'vendor/' + 'long-name-'.repeat(12) + '<script>text</script>';
    const free = {
        provider: 'DeepInfra', model: modelName, calls: 1, input_tokens: 234, output_tokens: 20,
        total_tokens: 254, cost_usd: 0, token_reported_calls: 1, cost_reported_calls: 1,
    };
    const paid = {
        ...free, model: 'vendor/weather', input_tokens: 1000, output_tokens: 30, total_tokens: 1030, cost_usd: 0.001234,
    };
    const codex = {
        ...free, provider: 'codex', model: 'gpt-fixture', calls: 2, input_tokens: 10, output_tokens: 5,
        total_tokens: 15, cost_usd: null, token_reported_calls: 1, cost_reported_calls: 0,
    };
    const upstream = {...paid, calls: 2, input_tokens: 1234, output_tokens: 50, total_tokens: 1284,
        token_reported_calls: 2, cost_reported_calls: 2};
    const overview = {
        total: {...upstream, calls: 4, input_tokens: 1244, output_tokens: 55, total_tokens: 1299, token_reported_calls: 3},
        providers: [upstream, codex], models: [free, paid, codex], legacy_entries: 1,
    };
    const runUsage = {total: free, providers: [free], models: [free], legacy_entries: 0};
    await mockMiniApp(context, ({action, data}) => {
        switch (action) {
            case 'status': return {user: '1', requests: [], tasks: [], time_zone: 'UTC'};
            case 'audit-sessions': return {sessions: [run], next_before: null, usage: overview,
                delivery: {runs: 2, completed_runs: 1, checked_off_tasks: 0, delivered_runs: 1,
                    average_first_feedback_ms: 500, average_result_delivery_ms: 3100}};
            case 'audit-session':
                assert.deepEqual(data, {id: 42});
                return {session: run, next_after: null, usage: runUsage, delivery: {
                    timing: {result_status: 'delivered', queue_wait_ms: 200, first_feedback_ms: 500,
                        result_delivery_ms: 3100}, receipts: [{kind: 'reply', state: 'sent'}],
                }, entries: [
                    {id: 1, at: started, kind: 'model_call', name: modelName, status: 'completed',
                        detail: {usage: {...free, reasoning_tokens: 12}}},
                ]};
            default: assert.fail('Unexpected Mini App action ' + action);
        }
    });
    const values = group => group.locator('.audit-metrics').first().locator('dd').allTextContents();
    try {
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Audit', exact: true}).click();
        const panel = page.locator('.audit-usage');
        await panel.getByRole('heading', {name: 'Last 7 days'}).waitFor();
        assert.deepEqual(await values(panel), ['$0.001234', '1,299', '4']);
        const deliverySummary = page.locator('.audit-delivery-summary');
        await deliverySummary.getByRole('heading', {name: 'Runs and delivery'}).waitFor();
        assert.deepEqual(await values(deliverySummary), ['1', '0', '1', '500 ms', '3.1 s']);
        await panel.getByText(/Cost reported for 2 of 4 calls.*Tokens reported for 3 of 4 calls.*Earlier calls/).waitFor();
        const breakdown = panel.locator('.audit-breakdown');
        await breakdown.waitFor({state: 'hidden'});
        await panel.getByText('Usage breakdown', {exact: true}).click();
        await breakdown.waitFor();
        assert.deepEqual(await values(breakdown), ['1,244', '55']);
        const split = provider => breakdown.locator('.audit-provider').filter({has: page.getByText(provider, {exact: true})});
        const provider = split('DeepInfra');
        assert.deepEqual(await values(provider), ['$0.001234', '1,234', '50', '1,284']);
        const model = name => provider.getByRole('listitem').filter({has: page.getByText(name, {exact: true})});
        assert.deepEqual(await values(model(modelName)), ['$0.00', '234', '20', '254']);
        assert.deepEqual(await values(model('vendor/weather')), ['$0.001234', '1,000', '30', '1,030']);
        assert.deepEqual(await values(split('Codex')), ['Not reported', '10', '5', '15']);
        assert.equal(await panel.locator('script').count(), 0);

        await page.getByText('Look up the forecast', {exact: true}).click();
        const session = page.locator('.audit-run-usage');
        await session.getByRole('heading', {name: 'Run usage', exact: true}).waitFor();
        assert.deepEqual(await values(session), ['$0.00', '254', '1']);
        const delivery = page.locator('.audit-delivery-run');
        await delivery.getByRole('heading', {name: 'Telegram delivery'}).waitFor();
        assert.deepEqual(await values(delivery), ['Confirmed', '200 ms', '500 ms', '3.1 s']);
        const runBreakdown = session.locator('.audit-breakdown');
        await runBreakdown.waitFor({state: 'hidden'});
        await session.getByText('Usage breakdown', {exact: true}).click();
        assert.deepEqual(await values(runBreakdown), ['234', '20']);
        const entry = page.locator('.audit-timeline > li');
        const modelCall = entry.locator('summary');
        await modelCall.waitFor();
        await entry.locator('.audit-metrics').waitFor({state: 'hidden'});
        await modelCall.click();
        await entry.locator('.audit-metrics').waitFor();
        assert.deepEqual(await values(entry), ['$0.00', '234', '20', '254', '12']);
        await entry.getByText('Reasoning tokens (included in output)', {exact: true}).waitFor();
        assert.equal(await entry.locator('script').count(), 0);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);

        const lightSurface = await panel.evaluate(element => getComputedStyle(element).backgroundColor);
        await page.emulateMedia({colorScheme: 'dark'});
        assert.notEqual(await panel.evaluate(element => getComputedStyle(element).backgroundColor), lightSurface);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
    } finally {
        await browser.close();
    }
});
test('OpenRouter server tool toggles save independently and restore on rejection', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const context = await browser.newContext({viewport: {width: 320, height: 620}});
        const page = await context.newPage();
        const enabled = {web_search: true, web_fetch: true, datetime: true, search_models: true};
        const saved = [];
        await mockMiniApp(context, ({action, data}) => {
            if (action === 'status') return {user: '1', requests: [], tasks: [], openrouter_server_tools: enabled};
            assert.equal(action, 'openrouter-server-tool');
            saved.push(data);
            enabled[data.tool] = data.enabled;
            return {ok: true};
        });
        await page.route('**/api/mini', async route => {
            const {action, data} = route.request().postDataJSON();
            if (action !== 'openrouter-server-tool' || data.tool !== 'web_fetch') return route.fallback();
            return route.fulfill({status: 503, json: {error: 'Could not save this setting.'}});
        });
        await page.goto('https://tinybot.test/mini');
        await page.getByRole('button', {name: 'Model', exact: true}).click();
        const search = page.getByRole('checkbox', {name: 'Web search', exact: true});
        const fetch = page.getByRole('checkbox', {name: 'Web fetch', exact: true});
        await search.uncheck();
        await page.getByText('Web search disabled.').waitFor();
        assert.deepEqual(saved, [{tool: 'web_search', enabled: false}]);
        await fetch.uncheck();
        await page.getByText('Could not save this setting.').waitFor();
        assert.equal(await fetch.isChecked(), true);
        assert.equal(await page.getByRole('checkbox', {name: 'Date and time'}).isChecked(), true);
        assert.equal(await page.getByRole('checkbox', {name: 'Search models'}).isChecked(), true);
        await page.reload();
        await page.getByRole('button', {name: 'Model', exact: true}).click();
        assert.equal(await search.isChecked(), false);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 320);
    } finally {
        await browser.close();
    }
});

test('operator invitations and guest shared memory respect the signed account view', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const state = {time_zone: 'UTC', provider: 'openrouter', requests: [], tasks: [], agents: []};
        const ownerContext = await browser.newContext({viewport: {width: 320, height: 640}});
        await mockMiniApp(ownerContext, ({action, data}) => {
            if (action === 'status') return {...state, user: '1', operator: true};
            if (action === 'list-invites') return {pending: [], guests: [{
                id: '2', display_name: 'Guest Sample', setup_complete: true, suspended: false, accepted_at: 1795526400,
                active_day: null, runs_started: 1, runs_completed: 1, tokens: null, cost_usd: 0,
            }]};
            if (action === 'create-invite') {
                assert.deepEqual(data, {});
                return {command: '/start fixture-invite'};
            }
            throw new Error('Unexpected owner action ' + action);
        });
        const owner = await ownerContext.newPage();
        await owner.goto('https://tinybot.test/mini');
        await owner.getByRole('button', {name: 'Invites', exact: true}).click();
        await owner.getByRole('heading', {name: 'Guest Sample'}).waitFor();
        assert.equal(await owner.getByText('Not reported', {exact: true}).count(), 1);
        await owner.getByRole('button', {name: 'Create invitation'}).click();
        await owner.getByText('/start fixture-invite', {exact: true}).waitFor();

        let joined = false;
        const guestContext = await browser.newContext({viewport: {width: 320, height: 640}});
        await mockMiniApp(guestContext, ({action, data}) => {
            if (action === 'status') return {...state, user: '2', operator: false, shared_memory_available: true};
            if (action === 'list-memories') return {entries: [], next_before: null};
            if (action === 'shared-memory-pairs') return {pairs: [{
                id: 'fixture-pair', peer: '1', joined, peer_joined: true, ready: joined,
            }]};
            if (action === 'join-shared-memory') {
                assert.deepEqual(data, {pair: 'fixture-pair'});
                joined = true;
                return {ok: true};
            }
            if (action === 'shared-memory-notes') return {notes: [{id: 'note', body: 'A published reference.', own: false}]};
            throw new Error('Unexpected guest action ' + action);
        });
        const guest = await guestContext.newPage();
        await guest.goto('https://tinybot.test/mini');
        await guest.getByRole('button', {name: 'Memory', exact: true}).click();
        assert.equal(await guest.getByRole('button', {name: 'Invites', exact: true}).count(), 0);
        await guest.getByRole('button', {name: 'Join shared memory'}).click();
        await guest.getByText('A published reference.', {exact: true}).waitFor();
        assert.equal(await guest.getByRole('button', {name: 'Remove shared note'}).count(), 0);
    } finally {
        await browser.close();
    }
});
