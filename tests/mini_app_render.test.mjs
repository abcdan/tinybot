import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {test} from 'node:test';
import {chromium} from 'playwright';

async function mockMiniApp(context, respond) {
    await context.route('https://telegram.org/**', route => route.fulfill({
        contentType: 'application/javascript',
        body: 'window.Telegram = {WebApp: {initData: "fixture", ready() {}, close() { window.miniAppClosed = true; }}};'
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
