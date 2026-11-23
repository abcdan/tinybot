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
        const file = path === '/mini' ? 'mini.html' : path.slice(1);
        if (!/^[a-z0-9-]+\.(html|css|js)$/.test(file)) return route.abort();
        const type = file.endsWith('.css') ? 'text/css' : file.endsWith('.html') ? 'text/html' : 'application/javascript';
        return route.fulfill({contentType: type, body: await readFile('/opt/tinybot/web/' + file, 'utf8')});
    });
}

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
            .getByRole('button', {name: 'Switch to task'}).click();
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
        assert.equal(await page.getByText('Message runs and their model and tool calls are kept for 7 days.').count(), 1);
        assert.equal(requests.some(({action}) => action === 'audit-session'), false);
        await page.getByText('Find the weather', {exact: true}).click();
        await page.getByRole('button', {name: 'Load more entries'}).click();
        await page.getByText('Sunny <script>safe text</script>', {exact: true}).waitFor();
        const entries = await page.locator('.audit-timeline > li').allTextContents();
        assert.equal(entries.length, 3);
        assert.match(entries[0], /Model · fixture\/model.*Started/s);
        assert.match(entries[1], /Tool · search_web.*Started.*Arguments.*Weather forecast/s);
        assert.match(entries[2], /Tool · search_web.*Completed.*Result.*Sunny/s);
        assert.equal(await page.locator('.audit-timeline script').count(), 0);
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
