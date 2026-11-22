import assert from 'node:assert/strict';
import {test} from 'node:test';
import {chromium} from 'playwright';
import {describe,populate,publicUrl} from './forms.mjs';
import {allowsRead, indexedUrl, visit} from './server.mjs';

test('public URL parsing accepts HTTPS and rejects credentials, fragments and alternate ports', () => {
    assert.equal(publicUrl('https://example.org/form').href, 'https://example.org/form');
    for (const url of ['http://example.org/', 'https://example.org:444/', 'https://user@example.org/', 'https://example.org/#private']) {
        assert.throws(() => publicUrl(url));
    }
});

test('a website response explains challenge pages and other HTTP failures', async () => {
    const url = new URL('https://fixture.example/');
    const pageWith = (status, headers = {}) => ({
        goto: async () => ({status: () => status, headers: () => headers, ok: () => status === 200}),
        url: () => url.href,
    });
    assert.equal((await visit(pageWith(200), url)).href, url.href);
    await assert.rejects(visit(pageWith(403, {'cf-mitigated': 'challenge'}), url), /Cloudflare requires verification/);
    await assert.rejects(visit(pageWith(404), url), /returned HTTP 404/);
});

test('rendering allows page resources while rejecting write methods', () => {
    assert.equal(allowsRead('GET', 'document', false), true);
    assert.equal(allowsRead('GET', 'script', false), false);
    assert.equal(allowsRead('GET', 'script', true), true);
    assert.equal(allowsRead('GET', 'fetch', true), true);
    assert.equal(allowsRead('HEAD', 'xhr', true), true);
    assert.equal(allowsRead('POST', 'fetch', true), false);
    assert.equal(allowsRead('PUT', 'xhr', true), false);
    assert.equal(allowsRead('GET', 'websocket', true), false);
});

test('indexed links use the published website rather than a search provider redirect', () => {
    const target = 'https://core.telegram.org/bots/api';
    const bing = `https://www.bing.com/ck/a?u=a1${Buffer.from(target).toString('base64url')}`;
    const google = `https://www.google.com/url?q=${encodeURIComponent(target)}`;
    assert.equal(indexedUrl(bing, 'www.bing.com'), target);
    assert.equal(indexedUrl(google, 'www.google.com'), target);
    assert.throws(() => indexedUrl('https://www.bing.com/ck/a?u=bad', 'www.bing.com'));
    assert.throws(() => indexedUrl('https://www.google.com/url?q=http://example.org', 'www.google.com'));
});

test('a native form can be inspected and filled and a changed target invalidates the approval', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({javaScriptEnabled: false});
    const page = await context.newPage();
    let target = '/send';
    const posts = [];
    await page.route('https://fixture.example/**', request => {
        const method = request.request().method();
        if (method === 'POST') {
            posts.push(request.request().postData());
            return request.fulfill({status: 204});
        }
        return request.fulfill({contentType: 'text/html', body:
            '<form method="post" action="' + target + '">' +
            '<input type="hidden" name="nonce" value="test">' +
            '<label>Contact email<input name="contact" type="email" required></label>' +
            '<label>Topic<select name="topic"><option value="sales">Sales</option></select></label></form>'
        });
    });
    try {
        await page.goto('https://fixture.example/form');
        const pinned = await describe(page, 0);
        assert.deepEqual(pinned.fields.map(({name, type}) => [name, type]), [['contact', 'email'], ['topic', 'select']]);
        await assert.rejects(populate(page, pinned, {contact: 'not-an-email'}));
        await page.goto('https://fixture.example/form');
        await populate(page, pinned, {contact: 'test@example.org', topic: 'sales'});
        const response = page.waitForResponse(event => event.request().method() === 'POST');
        await page.evaluate(() => HTMLFormElement.prototype.submit.call(document.forms[0]));
        await response;
        assert.deepEqual(posts, ['nonce=test&contact=test%40example.org&topic=sales']);
        target = '/different';
        await page.goto('https://fixture.example/form');
        await assert.rejects(populate(page, pinned, {contact: 'test@example.org'}), /changed the form/);
        assert.equal(posts.length, 1);
    } finally {
        await browser.close();
    }
});
