import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {test} from 'node:test';
import {chromium} from 'playwright';

test('a generated Mini App uses shared controls and submits details for masked review', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let received;
    await context.route('https://telegram.org/**', route => route.fulfill({
        contentType: 'application/javascript',
        body: 'window.Telegram = {WebApp: {initData: "fixture", ready() {}}};'
    }));
    await context.route('https://tinybot.test/**', async route => {
        const path = new URL(route.request().url()).pathname;
        if (path === '/api/mini') {
            const request = route.request().postDataJSON();
            assert.equal(request.init, 'fixture');
            if (request.action === 'submit-form') received = request.data;
            const details = {destination: 'https://forms.example/submit'};
            const requests = received ?
                [{id: 'review', kind: 'form-submit', content: {...details, preview: 'Email address: ••••'}}] :
                [{id: 'entry', kind: 'form', content: {...details, fields: [{
                    name: 'email', label: 'Email address', type: 'email', required: true,
                    options: [], private: true, hint: 'For example, jamie@example.com.'
                }]}}];
            return route.fulfill({json: {user: '1', provider: 'openrouter', requests, tasks: []}});
        }
        const file = path === '/mini' ? 'mini.html' : path.slice(1);
        if (!/^[a-z0-9-]+\.(html|css|js)$/.test(file)) return route.abort();
        const type = file.endsWith('.css') ? 'text/css' : file.endsWith('.html') ? 'text/html' : 'application/javascript';
        return route.fulfill({contentType: type, body: await readFile('/opt/tinybot/web/' + file, 'utf8')});
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
